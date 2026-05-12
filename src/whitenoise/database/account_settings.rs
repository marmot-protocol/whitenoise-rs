//! Database operations for account settings.
//!
//! Lives in the per-account SQLite file as a singleton row (`id = 1`). The
//! account's pubkey is implicit — it's whichever account owns the file.

use chrono::{DateTime, Utc};

use super::account_db::AccountDatabase;
use super::utils::parse_timestamp;
use crate::perf_span;
use crate::whitenoise::account_settings::AccountSettings;

/// Local row shape (no `account_pubkey` column — implied by the file).
struct LocalRow {
    id: i64,
    notifications_enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for LocalRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let notifications_int: i64 = row.try_get("notifications_enabled")?;
        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;
        Ok(Self {
            id,
            notifications_enabled: notifications_int != 0,
            created_at,
            updated_at,
        })
    }
}

impl AccountSettings {
    /// Returns the settings for the account that owns `db`, creating a default
    /// row (notifications enabled) if none exists.
    pub(crate) async fn find_or_create(db: &AccountDatabase) -> Result<Self, sqlx::Error> {
        let _span = perf_span!("db::account_settings_find_or_create");
        let now = Utc::now().timestamp_millis();

        let row: LocalRow = match sqlx::query_as::<_, LocalRow>(
            "INSERT INTO account_settings (id, notifications_enabled, created_at, updated_at)
             VALUES (1, 1, ?, ?)
             RETURNING *",
        )
        .bind(now)
        .bind(now)
        .fetch_one(&db.inner.pool)
        .await
        {
            Ok(row) => row,
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                sqlx::query_as::<_, LocalRow>("SELECT * FROM account_settings WHERE id = 1")
                    .fetch_one(&db.inner.pool)
                    .await?
            }
            Err(e) => return Err(e),
        };

        Ok(Self::from_local_row(db, row))
    }

    /// Returns whether notifications are enabled for the account that owns `db`.
    /// Returns `true` (the default) when no row exists.
    pub(crate) async fn notifications_enabled(db: &AccountDatabase) -> Result<bool, sqlx::Error> {
        let _span = perf_span!("db::account_settings_notifications_enabled");
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT notifications_enabled FROM account_settings WHERE id = 1")
                .fetch_optional(&db.inner.pool)
                .await?;

        Ok(row.map(|(v,)| v != 0).unwrap_or(true))
    }

    /// Sets `notifications_enabled` for the account that owns `db` and returns
    /// the updated settings. Creates a row if none exists.
    pub(crate) async fn update_notifications_enabled(
        db: &AccountDatabase,
        enabled: bool,
    ) -> Result<Self, sqlx::Error> {
        let _span = perf_span!("db::account_settings_update_notifications");
        let now = Utc::now().timestamp_millis();
        let enabled_int: i64 = if enabled { 1 } else { 0 };

        let row: LocalRow = sqlx::query_as::<_, LocalRow>(
            "INSERT INTO account_settings (id, notifications_enabled, created_at, updated_at)
             VALUES (1, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 notifications_enabled = excluded.notifications_enabled,
                 updated_at = excluded.updated_at
             RETURNING *",
        )
        .bind(enabled_int)
        .bind(now)
        .bind(now)
        .fetch_one(&db.inner.pool)
        .await?;

        Ok(Self::from_local_row(db, row))
    }

    /// Construct an [`AccountSettings`] from a local-DB row plus the owning
    /// account pubkey (which lives on the database handle, not the row).
    fn from_local_row(db: &AccountDatabase, row: LocalRow) -> Self {
        Self {
            id: Some(row.id),
            account_pubkey: *db.account_pubkey(),
            notifications_enabled: row.notifications_enabled,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::Keys;
    use tempfile::TempDir;

    /// Build a per-account DB with the post-move local schema applied
    /// directly. Bypasses the migration framework — that's covered in the
    /// migration's own tests; here we exercise the ops in isolation.
    async fn setup() -> (AccountDatabase, TempDir) {
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let path = dir.path().join("acct.db");
        let db = AccountDatabase::new(pubkey, path).await.unwrap();

        // `Database::new` already loaded the shared `fresh_schema.sql` against
        // this per-account pool (Phase 18a transitional behaviour). Replace
        // the shared-shape `account_settings` with the post-move shape so
        // these tests exercise the new schema.
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

        (db, dir)
    }

    #[tokio::test]
    async fn find_or_create_creates_default_row() {
        let (db, _dir) = setup().await;
        let settings = AccountSettings::find_or_create(&db).await.unwrap();
        assert_eq!(settings.id, Some(1));
        assert_eq!(settings.account_pubkey, *db.account_pubkey());
        assert!(settings.notifications_enabled);
    }

    #[tokio::test]
    async fn find_or_create_returns_existing_row() {
        let (db, _dir) = setup().await;
        let first = AccountSettings::find_or_create(&db).await.unwrap();
        let second = AccountSettings::find_or_create(&db).await.unwrap();
        assert_eq!(first.id, second.id);
    }

    #[tokio::test]
    async fn notifications_enabled_defaults_to_true_when_no_row() {
        let (db, _dir) = setup().await;
        assert!(AccountSettings::notifications_enabled(&db).await.unwrap());
    }

    #[tokio::test]
    async fn update_notifications_enabled_persists_value() {
        let (db, _dir) = setup().await;
        AccountSettings::update_notifications_enabled(&db, false)
            .await
            .unwrap();
        assert!(!AccountSettings::notifications_enabled(&db).await.unwrap());
    }

    #[tokio::test]
    async fn update_notifications_enabled_toggles_and_returns_updated() {
        let (db, _dir) = setup().await;

        let disabled = AccountSettings::update_notifications_enabled(&db, false)
            .await
            .unwrap();
        assert!(!disabled.notifications_enabled);

        let enabled = AccountSettings::update_notifications_enabled(&db, true)
            .await
            .unwrap();
        assert!(enabled.notifications_enabled);
        assert!(enabled.updated_at >= disabled.updated_at);
    }
}
