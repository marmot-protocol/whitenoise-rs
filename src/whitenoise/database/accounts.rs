use chrono::Utc;
use nostr_sdk::PublicKey;
use std::collections::HashSet;
use std::str::FromStr;

use super::{Database, DatabaseError, utils::parse_timestamp};
use crate::{
    WhitenoiseError, perf_instrument,
    whitenoise::{
        accounts::{Account, AccountType},
        users::User,
    },
};

impl<'r, R> sqlx::FromRow<'r, R> for Account
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let pubkey_str: String = row.try_get("pubkey")?;
        let user_id: i64 = row.try_get("user_id")?;
        let account_type_str: String = row.try_get("account_type")?;

        // Parse pubkey from hex string
        let pubkey = PublicKey::parse(&pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
            index: "pubkey".to_string(),
            source: Box::new(e),
        })?;

        // Parse account_type from string
        let account_type =
            AccountType::from_str(&account_type_str).map_err(|e| sqlx::Error::ColumnDecode {
                index: "account_type".to_string(),
                source: Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            })?;

        let last_synced_at = match row.try_get::<Option<i64>, _>("last_synced_at")? {
            Some(_) => Some(parse_timestamp(row, "last_synced_at")?),
            None => None,
        };

        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(Self {
            id: Some(id),
            pubkey,
            user_id,
            account_type,
            last_synced_at,
            created_at,
            updated_at,
        })
    }
}

impl Account {
    /// Loads all accounts from the database.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns a `Vec<Account>` containing all accounts in the database on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database query fails.
    #[perf_instrument("db::accounts")]
    pub(crate) async fn all(database: &Database) -> Result<Vec<Account>, WhitenoiseError> {
        let accounts = sqlx::query_as::<_, Self>("SELECT * FROM accounts")
            .fetch_all(&database.pool)
            .await
            .map_err(DatabaseError::Sqlx)?;

        Ok(accounts)
    }

    /// Gets the oldest account from the database.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns the oldest `Account` from the database on success. If no account exists, return None.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database query fails.
    ///
    /// Test-only: production code resolves accounts explicitly by pubkey.
    #[cfg(test)]
    pub(crate) async fn first(database: &Database) -> Result<Option<Account>, WhitenoiseError> {
        let account = sqlx::query_as::<_, Self>(
            "SELECT * FROM accounts ORDER BY created_at ASC, id ASC LIMIT 1",
        )
        .fetch_optional(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(account)
    }

    /// Finds an account by its public key.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - A reference to the `PublicKey` to search for
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns the `Account` associated with the provided public key on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError::AccountNotFound`] if no account with the given public key exists.
    #[perf_instrument("db::accounts")]
    pub(crate) async fn find_by_pubkey(
        pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Account, WhitenoiseError> {
        let account = sqlx::query_as::<_, Self>("SELECT * FROM accounts WHERE pubkey = ?")
            .bind(pubkey.to_hex().as_str())
            .fetch_one(&database.pool)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => WhitenoiseError::AccountNotFound,
                other => WhitenoiseError::Database(DatabaseError::Sqlx(other)),
            })?;

        Ok(account)
    }

    /// Gets the user associated with this account.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns the `User` associated with this account on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError::AccountNotFound`] if the associated user is not found.
    #[perf_instrument("db::accounts")]
    pub(crate) async fn user(&self, database: &Database) -> Result<User, WhitenoiseError> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
            .bind(self.user_id)
            .fetch_one(&database.pool)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => WhitenoiseError::MissingUserReference,
                other => WhitenoiseError::Database(DatabaseError::Sqlx(other)),
            })?;
        Ok(user)
    }

    /// Updates this account's follow list from a ContactList event.
    ///
    /// Replaces all current follows with the contacts from the event. The
    /// per-account `account_follows` table is rewritten atomically; new
    /// `users` rows are created in the shared DB for any contacts we hadn't
    /// seen before so the caller can fetch their metadata.
    pub(crate) async fn update_follows_from_event(
        &self,
        contacts: Vec<nostr_sdk::PublicKey>,
        session: &crate::whitenoise::session::AccountSession,
        database: &Database,
    ) -> Result<Vec<nostr_sdk::PublicKey>, WhitenoiseError> {
        // Deduplicate contacts.
        let unique_contacts: Vec<_> = contacts
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let contacts_len = unique_contacts.len();

        // Resolve / create user rows in the shared DB so we can return the
        // newly-created subset for downstream metadata fetches. Done outside
        // the per-account transaction; idempotent on retry.
        let mut tx = database.pool.begin().await.map_err(DatabaseError::Sqlx)?;
        let mut newly_created_users = Vec::new();
        for pubkey in &unique_contacts {
            let (_user, newly_created) = User::find_or_create_by_pubkey_tx(pubkey, &mut tx).await?;
            if newly_created {
                newly_created_users.push(*pubkey);
                tracing::debug!("Created new user for follow: {}", pubkey.to_hex());
            }
        }
        tx.commit().await.map_err(DatabaseError::Sqlx)?;

        // Replace the per-account follow set in one transaction.
        session.repos.follows.replace_all(&unique_contacts).await?;

        tracing::debug!(
            target: "whitenoise::database::accounts::update_follows_from_event",
            "Updated follows for account {} with {} contacts, {} newly created users",
            self.pubkey.to_hex(),
            contacts_len,
            newly_created_users.len()
        );

        Ok(newly_created_users)
    }

    /// Saves this account to the database and returns the saved account with up-to-date values.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns the saved `Account` with values as stored in the database, including the
    /// database-assigned `id` and the actual `updated_at` timestamp.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operation fails.
    pub(crate) async fn save(&self, database: &Database) -> Result<Account, WhitenoiseError> {
        let account = sqlx::query_as::<_, Self>(
            "INSERT INTO accounts (pubkey, user_id, account_type, last_synced_at, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(pubkey) DO UPDATE
             SET user_id = excluded.user_id,
                 account_type = excluded.account_type,
                 last_synced_at = excluded.last_synced_at,
                 updated_at = ?
             RETURNING *",
        )
        .bind(self.pubkey.to_hex().as_str())
        .bind(self.user_id)
        .bind(self.account_type.to_string())
        .bind(self.last_synced_at.map(|ts| ts.timestamp_millis()))
        .bind(self.created_at.timestamp_millis())
        .bind(self.updated_at.timestamp_millis())
        .bind(Utc::now().timestamp_millis())
        .fetch_one(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(account)
    }

    /// Deletes this account from the database.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError::AccountNotFound`] if the account doesn't exist in the database.
    /// Returns other [`WhitenoiseError`] variants if the database operation fails.
    pub(crate) async fn delete(&self, database: &Database) -> Result<(), WhitenoiseError> {
        let result = sqlx::query("DELETE FROM accounts WHERE pubkey = ?")
            .bind(self.pubkey.to_hex())
            .execute(&database.pool)
            .await?;

        tracing::debug!(target: "whitenoise::delete_account", "Account removed from database for pubkey: {}", self.pubkey.to_hex());

        if result.rows_affected() < 1 {
            Err(WhitenoiseError::AccountNotFound)
        } else {
            Ok(())
        }
    }

    /// Advances `last_synced_at` to the provided `created_ms` if it's newer.
    /// Does nothing if the existing value is greater or equal. Also updates `updated_at`.
    pub(crate) async fn update_last_synced_max(
        pubkey: &PublicKey,
        created_ms: i64,
        database: &Database,
    ) -> Result<(), WhitenoiseError> {
        let now_ms = Utc::now().timestamp_millis();
        sqlx::query(
            "UPDATE accounts
             SET last_synced_at = CASE
                 WHEN last_synced_at IS NULL OR last_synced_at < ? THEN ?
                 ELSE last_synced_at
             END,
             updated_at = ?
             WHERE pubkey = ?",
        )
        .bind(created_ms)
        .bind(created_ms)
        .bind(now_ms)
        .bind(pubkey.to_hex())
        .execute(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;

    use crate::whitenoise::test_utils::{create_mock_whitenoise, create_test_account};

    use super::*;
    use sqlx::sqlite::SqliteRow;
    use sqlx::{FromRow, SqlitePool};

    // Helper function to create a test database
    async fn setup_test_db() -> SqlitePool {
        let pool = SqlitePool::connect(":memory:").await.unwrap();

        // Create the accounts table with the schema from migration
        sqlx::query(
            "CREATE TABLE accounts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pubkey TEXT NOT NULL,
                user_id INTEGER NOT NULL,
                account_type TEXT NOT NULL DEFAULT 'local',
                last_synced_at INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create users table
        sqlx::query(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pubkey TEXT NOT NULL UNIQUE,
                metadata TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                metadata_known_at INTEGER,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create account_follows table
        sqlx::query(
            "CREATE TABLE account_follows (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id INTEGER NOT NULL,
                user_id INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                FOREIGN KEY (account_id) REFERENCES accounts (id),
                FOREIGN KEY (user_id) REFERENCES users (id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Add unique constraint to prevent duplicate follows
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_account_follows_unique
             ON account_follows(account_id, user_id)",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    #[tokio::test]
    async fn test_account_row_from_row_invalid_pubkey() {
        let pool = setup_test_db().await;

        let invalid_pubkeys = [
            "not_a_pubkey",
            "too_short",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz", // Invalid hex
            "",
        ];

        let test_timestamp = chrono::Utc::now().timestamp_millis();

        for (i, invalid_pubkey) in invalid_pubkeys.iter().enumerate() {
            let test_user_id = (i + 1) as i64;

            // Insert invalid pubkey
            sqlx::query(
                "INSERT INTO accounts (pubkey, user_id, created_at, updated_at) VALUES (?, ?, ?, ?)",
            )
            .bind(invalid_pubkey)
            .bind(test_user_id)
            .bind(test_timestamp)
            .bind(test_timestamp)
            .execute(&pool)
            .await
            .unwrap();

            // Test from_row implementation should fail
            let row: SqliteRow = sqlx::query("SELECT * FROM accounts WHERE pubkey = ?")
                .bind(invalid_pubkey)
                .fetch_one(&pool)
                .await
                .unwrap();

            let result = Account::from_row(&row);
            assert!(result.is_err());

            if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
                assert_eq!(index, "pubkey");
            } else {
                panic!("Expected ColumnDecode error for pubkey");
            }

            // Clean up
            sqlx::query("DELETE FROM accounts WHERE pubkey = ?")
                .bind(invalid_pubkey)
                .execute(&pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_account_row_from_row_invalid_timestamps() {
        let pool = setup_test_db().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_user_id = 777i64;
        let valid_timestamp = chrono::Utc::now().timestamp_millis();
        let invalid_timestamp = i64::MAX; // This will be too large for DateTime conversion

        // Test invalid last_synced_at timestamp
        sqlx::query(
            "INSERT INTO accounts (pubkey, user_id, last_synced_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(test_pubkey.to_hex())
        .bind(test_user_id)
        .bind(invalid_timestamp)
        .bind(valid_timestamp)
        .bind(valid_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM accounts WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = Account::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "last_synced_at");
        } else {
            panic!("Expected ColumnDecode error for last_synced_at timestamp");
        }

        // Clean up and test invalid created_at timestamp
        sqlx::query("DELETE FROM accounts WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO accounts (pubkey, user_id, last_synced_at, created_at, updated_at) VALUES (?, ?, NULL, ?, ?)",
        )
        .bind(test_pubkey.to_hex())
        .bind(test_user_id)
        .bind(invalid_timestamp)
        .bind(valid_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM accounts WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = Account::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "created_at");
        } else {
            panic!("Expected ColumnDecode error for created_at timestamp");
        }

        // Clean up and test invalid updated_at timestamp
        sqlx::query("DELETE FROM accounts WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO accounts (pubkey, user_id, last_synced_at, created_at, updated_at) VALUES (?, ?, NULL, ?, ?)",
        )
        .bind(test_pubkey.to_hex())
        .bind(test_user_id)
        .bind(valid_timestamp)
        .bind(invalid_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM accounts WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = Account::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "updated_at");
        } else {
            panic!("Expected ColumnDecode error for updated_at timestamp");
        }
    }

    #[tokio::test]
    async fn test_save_account_success() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test account
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_user_id = 42i64;
        let test_last_synced = Some(chrono::Utc::now());
        let test_created_at = chrono::Utc::now();
        let test_updated_at = chrono::Utc::now();

        let account = Account {
            id: Some(1), // Will be overridden by database auto-increment
            pubkey: test_pubkey,
            user_id: test_user_id,
            account_type: AccountType::Local,
            last_synced_at: test_last_synced,
            created_at: test_created_at,
            updated_at: test_updated_at,
        };

        // Test save_account
        let result = account.save(&whitenoise.shared.database).await;
        assert!(result.is_ok());

        // Test that we can load it back (this verifies it was saved correctly)
        let loaded_account =
            Account::find_by_pubkey(&test_pubkey, &whitenoise.shared.database).await;
        assert!(loaded_account.is_ok());

        let loaded = loaded_account.unwrap();
        assert_eq!(loaded.pubkey, test_pubkey);
        assert_eq!(loaded.user_id, test_user_id);
        assert!(loaded.last_synced_at.is_some());
        assert_eq!(
            loaded.created_at.timestamp_millis(),
            test_created_at.timestamp_millis()
        );
        assert_eq!(
            loaded.updated_at.timestamp_millis(),
            test_updated_at.timestamp_millis()
        );
    }

    #[tokio::test]
    async fn test_save_account_with_null_last_synced() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_user_id = 123i64;
        let test_created_at = chrono::Utc::now();
        let test_updated_at = chrono::Utc::now();

        let account = Account {
            id: Some(1),
            pubkey: test_pubkey,
            user_id: test_user_id,
            account_type: AccountType::Local,
            last_synced_at: None, // Test with None
            created_at: test_created_at,
            updated_at: test_updated_at,
        };

        let result = account.save(&whitenoise.shared.database).await;
        assert!(result.is_ok());

        // Verify it was saved correctly by loading it back
        let loaded_account =
            Account::find_by_pubkey(&test_pubkey, &whitenoise.shared.database).await;
        assert!(loaded_account.is_ok());

        let loaded = loaded_account.unwrap();
        assert!(loaded.last_synced_at.is_none());
        assert_eq!(
            loaded.created_at.timestamp_millis(),
            test_created_at.timestamp_millis()
        );
        assert_eq!(
            loaded.updated_at.timestamp_millis(),
            test_updated_at.timestamp_millis()
        );
    }

    #[tokio::test]
    async fn test_load_account_not_found() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Try to load a non-existent account
        let non_existent_pubkey = nostr_sdk::Keys::generate().public_key();
        let result =
            Account::find_by_pubkey(&non_existent_pubkey, &whitenoise.shared.database).await;

        assert!(result.is_err());
        if let Err(WhitenoiseError::AccountNotFound) = result {
            // Expected error
        } else {
            panic!("Expected AccountNotFound error");
        }
    }

    #[tokio::test]
    async fn test_save_and_load_account_roundtrip() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test account with all fields
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_user_id = 555i64;
        let test_last_synced = Some(chrono::Utc::now());
        let test_created_at = chrono::Utc::now();
        let test_updated_at = chrono::Utc::now();

        let original_account = Account {
            id: Some(1),
            pubkey: test_pubkey,
            user_id: test_user_id,
            account_type: AccountType::Local,
            last_synced_at: test_last_synced,
            created_at: test_created_at,
            updated_at: test_updated_at,
        };

        // Save the account
        let save_result = original_account.save(&whitenoise.shared.database).await;
        assert!(save_result.is_ok());

        // Load the account back
        let loaded_account =
            Account::find_by_pubkey(&test_pubkey, &whitenoise.shared.database).await;
        assert!(loaded_account.is_ok());

        let account = loaded_account.unwrap();

        // Verify all fields match (except id which is set by database)
        assert_eq!(account.pubkey, original_account.pubkey);
        assert_eq!(account.user_id, original_account.user_id);
        assert_eq!(
            account.last_synced_at.map(|ts| ts.timestamp_millis()),
            original_account
                .last_synced_at
                .map(|ts| ts.timestamp_millis())
        );
        assert_eq!(
            account.created_at.timestamp_millis(),
            original_account.created_at.timestamp_millis()
        );
        assert_eq!(
            account.updated_at.timestamp_millis(),
            original_account.updated_at.timestamp_millis()
        );
    }

    #[tokio::test]
    async fn test_update_last_synced_max_sets_when_null() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pubkey = nostr_sdk::Keys::generate().public_key();
        let created_at = chrono::Utc::now();
        let updated_at = created_at;

        // Account with NULL last_synced_at
        let account = Account {
            id: Some(1),
            pubkey,
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at,
            updated_at,
        };
        account.save(&whitenoise.shared.database).await.unwrap();

        let before = Account::find_by_pubkey(&pubkey, &whitenoise.shared.database)
            .await
            .unwrap();

        let event_ms = chrono::Utc::now().timestamp_millis();
        Account::update_last_synced_max(&pubkey, event_ms, &whitenoise.shared.database)
            .await
            .unwrap();

        let after = Account::find_by_pubkey(&pubkey, &whitenoise.shared.database)
            .await
            .unwrap();

        assert!(after.last_synced_at.is_some());
        assert_eq!(after.last_synced_at.unwrap().timestamp_millis(), event_ms);
        assert!(after.updated_at.timestamp_millis() >= before.updated_at.timestamp_millis());
    }

    #[tokio::test]
    async fn test_update_last_synced_max_no_downgrade() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pubkey = nostr_sdk::Keys::generate().public_key();
        let base_ms = chrono::Utc::now().timestamp_millis();
        let initial_ms = base_ms + 5_000;
        let older_ms = base_ms;

        let account = Account {
            id: Some(1),
            pubkey,
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: chrono::DateTime::<chrono::Utc>::from_timestamp_millis(initial_ms),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        account.save(&whitenoise.shared.database).await.unwrap();

        let before = Account::find_by_pubkey(&pubkey, &whitenoise.shared.database)
            .await
            .unwrap();

        Account::update_last_synced_max(&pubkey, older_ms, &whitenoise.shared.database)
            .await
            .unwrap();

        let after = Account::find_by_pubkey(&pubkey, &whitenoise.shared.database)
            .await
            .unwrap();

        assert_eq!(after.last_synced_at.unwrap().timestamp_millis(), initial_ms);
        // updated_at is always refreshed by the helper
        assert!(after.updated_at.timestamp_millis() >= before.updated_at.timestamp_millis());
    }

    #[tokio::test]
    async fn test_update_last_synced_max_advances_when_newer() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pubkey = nostr_sdk::Keys::generate().public_key();
        let base_ms = chrono::Utc::now().timestamp_millis();
        let initial_ms = base_ms;
        let newer_ms = base_ms + 10_000;

        let account = Account {
            id: Some(1),
            pubkey,
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: chrono::DateTime::<chrono::Utc>::from_timestamp_millis(initial_ms),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        account.save(&whitenoise.shared.database).await.unwrap();

        let before = Account::find_by_pubkey(&pubkey, &whitenoise.shared.database)
            .await
            .unwrap();

        Account::update_last_synced_max(&pubkey, newer_ms, &whitenoise.shared.database)
            .await
            .unwrap();

        let after = Account::find_by_pubkey(&pubkey, &whitenoise.shared.database)
            .await
            .unwrap();

        assert_eq!(after.last_synced_at.unwrap().timestamp_millis(), newer_ms);
        assert!(after.updated_at.timestamp_millis() >= before.updated_at.timestamp_millis());
    }

    #[tokio::test]
    async fn test_first_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Should return None when no accounts exist
        let account = Account::first(&whitenoise.shared.database).await.unwrap();
        assert!(account.is_none());

        // If there's a single account, it should return that account
        let test_keys = nostr_sdk::Keys::generate();
        User::find_or_create_by_pubkey(&test_keys.public_key(), &whitenoise.shared.database)
            .await
            .unwrap();
        let (test_account, _) = Account::new(&whitenoise, Some(test_keys)).await.unwrap();
        test_account
            .save(&whitenoise.shared.database)
            .await
            .unwrap();
        let account = Account::first(&whitenoise.shared.database).await.unwrap();
        assert!(account.is_some());
        assert_eq!(account.unwrap().pubkey, test_account.pubkey);

        // If there's more than one account, it should still return the first one
        let test_keys2 = nostr_sdk::Keys::generate();
        User::find_or_create_by_pubkey(&test_keys2.public_key(), &whitenoise.shared.database)
            .await
            .unwrap();
        let (test_account2, _) = Account::new(&whitenoise, Some(test_keys2)).await.unwrap();
        test_account2
            .save(&whitenoise.shared.database)
            .await
            .unwrap();
        let account2 = Account::first(&whitenoise.shared.database).await.unwrap();
        assert!(account2.is_some());
        assert_eq!(account2.unwrap().pubkey, test_account.pubkey);

        // If that first account is deleted, it should return the second one
        test_account
            .delete(&whitenoise.shared.database)
            .await
            .unwrap();
        let account3 = Account::first(&whitenoise.shared.database).await.unwrap();
        assert!(account3.is_some());
        assert_eq!(account3.unwrap().pubkey, test_account2.pubkey);

        // If all accounts are deleted, it should return None
        test_account2
            .delete(&whitenoise.shared.database)
            .await
            .unwrap();
        let account4 = Account::first(&whitenoise.shared.database).await.unwrap();
        assert!(account4.is_none());
    }

    #[tokio::test]
    async fn test_save_and_load_external_account_type() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test account with External account type
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_user_id = 999i64;
        let test_created_at = chrono::Utc::now();
        let test_updated_at = chrono::Utc::now();

        let account = Account {
            id: Some(1),
            pubkey: test_pubkey,
            user_id: test_user_id,
            account_type: AccountType::External,
            last_synced_at: None,
            created_at: test_created_at,
            updated_at: test_updated_at,
        };

        // Save the account
        let result = account.save(&whitenoise.shared.database).await;
        assert!(result.is_ok());

        // Load the account back and verify account_type is preserved
        let loaded_account =
            Account::find_by_pubkey(&test_pubkey, &whitenoise.shared.database).await;
        assert!(loaded_account.is_ok());

        let loaded = loaded_account.unwrap();
        assert_eq!(loaded.pubkey, test_pubkey);
        assert_eq!(loaded.user_id, test_user_id);
        assert_eq!(loaded.account_type, AccountType::External);
        assert!(loaded.last_synced_at.is_none());
    }

    #[tokio::test]
    async fn test_delete_account_success() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;
        let saved = account.save(&whitenoise.shared.database).await.unwrap();

        // Verify it exists.
        assert!(
            Account::find_by_pubkey(&saved.pubkey, &whitenoise.shared.database)
                .await
                .is_ok()
        );

        // Delete it.
        saved.delete(&whitenoise.shared.database).await.unwrap();

        // Verify it's gone.
        assert!(
            Account::find_by_pubkey(&saved.pubkey, &whitenoise.shared.database)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_delete_account_not_found() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an unsaved account (not in DB).
        let keys = Keys::generate();
        let account = Account {
            id: None,
            pubkey: keys.public_key(),
            user_id: 999,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let result = account.delete(&whitenoise.shared.database).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), WhitenoiseError::AccountNotFound),
            "Deleting nonexistent account should return AccountNotFound"
        );
    }
}
