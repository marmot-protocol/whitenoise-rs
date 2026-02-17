//! Database operations for account settings.

use chrono::{DateTime, Utc};
use nostr_sdk::PublicKey;

use super::{Database, utils::parse_timestamp};
use crate::whitenoise::account_settings::AccountSettings;

/// Internal database row representation for account_settings table.
#[derive(Debug)]
struct AccountSettingsRow {
    id: i64,
    account_pubkey: PublicKey,
    notifications_enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for AccountSettingsRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;

        let account_pubkey_str: String = row.try_get("account_pubkey")?;
        let account_pubkey =
            PublicKey::parse(&account_pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
                index: "account_pubkey".to_string(),
                source: Box::new(e),
            })?;

        let notifications_int: i64 = row.try_get("notifications_enabled")?;
        let notifications_enabled = notifications_int != 0;

        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(Self {
            id,
            account_pubkey,
            notifications_enabled,
            created_at,
            updated_at,
        })
    }
}

impl From<AccountSettingsRow> for AccountSettings {
    fn from(row: AccountSettingsRow) -> Self {
        Self {
            id: Some(row.id),
            account_pubkey: row.account_pubkey,
            notifications_enabled: row.notifications_enabled,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

impl AccountSettings {
    /// Returns the settings for `pubkey`, creating a default row (notifications enabled)
    /// if none exists. Uses insert-first to avoid TOCTOU races.
    pub(crate) async fn find_or_create_for_pubkey(
        pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Self, sqlx::Error> {
        let now = Utc::now().timestamp_millis();

        match sqlx::query_as::<_, AccountSettingsRow>(
            "INSERT INTO account_settings (account_pubkey, notifications_enabled, created_at, updated_at)
             VALUES (?, 1, ?, ?)
             RETURNING *",
        )
        .bind(pubkey.to_hex())
        .bind(now)
        .bind(now)
        .fetch_one(&database.pool)
        .await
        {
            Ok(row) => Ok(row.into()),
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                let row = sqlx::query_as::<_, AccountSettingsRow>(
                    "SELECT * FROM account_settings WHERE account_pubkey = ?",
                )
                .bind(pubkey.to_hex())
                .fetch_one(&database.pool)
                .await?;
                Ok(row.into())
            }
            Err(e) => Err(e),
        }
    }

    /// Returns whether notifications are enabled for `pubkey`.
    /// Returns `true` (the default) when no row exists.
    pub(crate) async fn notifications_enabled_for_pubkey(
        pubkey: &PublicKey,
        database: &Database,
    ) -> Result<bool, sqlx::Error> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT notifications_enabled FROM account_settings WHERE account_pubkey = ?",
        )
        .bind(pubkey.to_hex())
        .fetch_optional(&database.pool)
        .await?;

        Ok(row.map(|(v,)| v != 0).unwrap_or(true))
    }

    /// Sets `notifications_enabled` for `pubkey` and returns the updated settings.
    /// Creates a row if none exists (upsert).
    pub(crate) async fn update_notifications_enabled(
        pubkey: &PublicKey,
        enabled: bool,
        database: &Database,
    ) -> Result<Self, sqlx::Error> {
        let now = Utc::now().timestamp_millis();
        let enabled_int: i64 = if enabled { 1 } else { 0 };

        let row = sqlx::query_as::<_, AccountSettingsRow>(
            "INSERT INTO account_settings (account_pubkey, notifications_enabled, created_at, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(account_pubkey) DO UPDATE SET
                 notifications_enabled = excluded.notifications_enabled,
                 updated_at = excluded.updated_at
             RETURNING *",
        )
        .bind(pubkey.to_hex())
        .bind(enabled_int)
        .bind(now)
        .bind(now)
        .fetch_one(&database.pool)
        .await?;

        Ok(row.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::*;
    use nostr_sdk::Keys;

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
    async fn test_find_or_create_creates_default_row() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&whitenoise.database, &keys.public_key()).await;

        let settings =
            AccountSettings::find_or_create_for_pubkey(&keys.public_key(), &whitenoise.database)
                .await
                .unwrap();

        assert!(settings.id.is_some());
        assert_eq!(settings.account_pubkey, keys.public_key());
        assert!(settings.notifications_enabled);
    }

    #[tokio::test]
    async fn test_find_or_create_returns_existing_row() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&whitenoise.database, &keys.public_key()).await;

        let first =
            AccountSettings::find_or_create_for_pubkey(&keys.public_key(), &whitenoise.database)
                .await
                .unwrap();
        let second =
            AccountSettings::find_or_create_for_pubkey(&keys.public_key(), &whitenoise.database)
                .await
                .unwrap();

        assert_eq!(first.id, second.id);
    }

    #[tokio::test]
    async fn test_notifications_enabled_defaults_to_true_when_no_row() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        let enabled = AccountSettings::notifications_enabled_for_pubkey(
            &keys.public_key(),
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(enabled);
    }

    #[tokio::test]
    async fn test_notifications_enabled_returns_stored_value() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&whitenoise.database, &keys.public_key()).await;

        AccountSettings::update_notifications_enabled(
            &keys.public_key(),
            false,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let enabled = AccountSettings::notifications_enabled_for_pubkey(
            &keys.public_key(),
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(!enabled);
    }

    #[tokio::test]
    async fn test_update_notifications_enabled_toggles_and_returns_updated() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&whitenoise.database, &keys.public_key()).await;

        let disabled = AccountSettings::update_notifications_enabled(
            &keys.public_key(),
            false,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(!disabled.notifications_enabled);
        assert!(disabled.id.is_some());

        let enabled = AccountSettings::update_notifications_enabled(
            &keys.public_key(),
            true,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(enabled.notifications_enabled);
        assert!(enabled.updated_at >= disabled.updated_at);
    }

    #[tokio::test]
    async fn test_row_to_model_conversion() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&whitenoise.database, &keys.public_key()).await;

        let settings =
            AccountSettings::find_or_create_for_pubkey(&keys.public_key(), &whitenoise.database)
                .await
                .unwrap();

        // Verify From<AccountSettingsRow> produced correct values
        assert!(settings.id.is_some());
        assert_eq!(settings.account_pubkey, keys.public_key());
        assert!(settings.notifications_enabled);
        assert!(settings.created_at <= Utc::now());
        assert!(settings.updated_at <= Utc::now());
    }
}
