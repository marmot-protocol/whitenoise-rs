//! Per-account repository for account settings.
//!
//! Wraps the [`AccountSettings`] DB functions over an [`AccountDatabase`] —
//! the account is implicit because the file owns the scope.

use std::sync::Arc;

use crate::whitenoise::account_settings::AccountSettings;
use crate::whitenoise::database::account_db::AccountDatabase;
use crate::whitenoise::error::Result;

/// Repository for account settings scoped to a single account.
#[derive(Clone, Debug)]
pub struct AccountSettingsRepo {
    db: Arc<AccountDatabase>,
}

impl AccountSettingsRepo {
    /// Construct a new [`AccountSettingsRepo`] over the given per-account DB.
    pub(crate) fn new(db: Arc<AccountDatabase>) -> Self {
        Self { db }
    }

    /// Return the settings for this account, creating a default row if none
    /// exists.
    pub async fn find_or_create(&self) -> Result<AccountSettings> {
        Ok(AccountSettings::find_or_create(&self.db).await?)
    }

    /// Return whether notifications are enabled for this account.
    ///
    /// Returns `true` (the default) when no settings row exists.
    pub async fn notifications_enabled(&self) -> Result<bool> {
        Ok(AccountSettings::notifications_enabled(&self.db).await?)
    }

    /// Set `notifications_enabled` for this account and return the updated
    /// settings.
    ///
    /// **Note:** this only updates the database row. It does **not** perform
    /// push-token reconciliation (sharing or removing the local push token
    /// from joined groups). Callers that need the full side-effectful behaviour
    /// should use `Whitenoise::update_notifications_enabled` instead, or handle
    /// push-token sync separately.
    pub async fn update_notifications_enabled(&self, enabled: bool) -> Result<AccountSettings> {
        Ok(AccountSettings::update_notifications_enabled(&self.db, enabled).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nostr_sdk::Keys;
    use tempfile::TempDir;

    use super::AccountSettingsRepo;
    use crate::whitenoise::database::account_db::AccountDatabase;

    async fn setup() -> (AccountSettingsRepo, TempDir) {
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let db = Arc::new(
            AccountDatabase::new(pubkey, dir.path().join("acct.db"))
                .await
                .unwrap(),
        );

        sqlx::query("DROP TABLE IF EXISTS account_settings")
            .execute(&db.inner.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE account_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                notifications_enabled INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(&db.inner.pool)
        .await
        .unwrap();

        (AccountSettingsRepo::new(db), dir)
    }

    #[tokio::test]
    async fn find_or_create_returns_defaults_for_new_account() {
        let (repo, _dir) = setup().await;
        let settings = repo.find_or_create().await.unwrap();
        assert!(settings.id.is_some());
        assert!(settings.notifications_enabled);
    }

    #[tokio::test]
    async fn find_or_create_is_idempotent() {
        let (repo, _dir) = setup().await;
        let first = repo.find_or_create().await.unwrap();
        let second = repo.find_or_create().await.unwrap();
        assert_eq!(first.id, second.id);
    }

    #[tokio::test]
    async fn notifications_enabled_defaults_to_true_when_no_row() {
        let (repo, _dir) = setup().await;
        assert!(repo.notifications_enabled().await.unwrap());
    }

    #[tokio::test]
    async fn update_notifications_enabled_persists_value() {
        let (repo, _dir) = setup().await;
        let updated = repo.update_notifications_enabled(false).await.unwrap();
        assert!(!updated.notifications_enabled);
        assert!(!repo.notifications_enabled().await.unwrap());
    }

    #[tokio::test]
    async fn update_notifications_enabled_round_trips() {
        let (repo, _dir) = setup().await;

        let disabled = repo.update_notifications_enabled(false).await.unwrap();
        assert!(!disabled.notifications_enabled);

        let enabled = repo.update_notifications_enabled(true).await.unwrap();
        assert!(enabled.notifications_enabled);
        assert!(enabled.updated_at >= disabled.updated_at);
    }
}
