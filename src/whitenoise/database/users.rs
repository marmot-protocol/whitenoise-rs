use chrono::{DateTime, Utc};
use nostr_sdk::{Metadata, PublicKey, RelayUrl};

use super::{Database, DatabaseError, relays::RelayRow, utils::parse_timestamp};
use crate::{
    WhitenoiseError,
    whitenoise::{
        relays::{Relay, RelayType},
        users::User,
    },
};

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub(crate) struct UserRow {
    // id is the primary key
    pub id: i64,
    // pubkey is the hex encoded nostr public key
    pub pubkey: PublicKey,
    // metadata is the JSONB column that stores the user metadata
    pub metadata: Metadata,
    // created_at is the timestamp of the user creation
    pub created_at: DateTime<Utc>,
    // updated_at is the timestamp of the last update
    pub updated_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for UserRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let pubkey_str: String = row.try_get("pubkey")?;
        let metadata_json: String = row.try_get("metadata")?;

        // Parse pubkey from hex string
        let pubkey = PublicKey::parse(&pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
            index: "pubkey".to_string(),
            source: Box::new(e),
        })?;

        // Parse metadata from JSON
        let metadata: Metadata =
            serde_json::from_str(&metadata_json).map_err(|e| sqlx::Error::ColumnDecode {
                index: "metadata".to_string(),
                source: Box::new(e),
            })?;

        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(UserRow {
            id,
            pubkey,
            metadata,
            created_at,
            updated_at,
        })
    }
}

impl From<UserRow> for User {
    fn from(val: UserRow) -> Self {
        User {
            id: Some(val.id),
            pubkey: val.pubkey,
            metadata: val.metadata,
            created_at: val.created_at,
            updated_at: val.updated_at,
        }
    }
}

impl User {
    #[cfg(test)]
    pub(crate) async fn all(database: &Database) -> Result<Vec<User>, WhitenoiseError> {
        let user_rows = sqlx::query_as::<_, UserRow>("SELECT * FROM users")
            .fetch_all(&database.pool)
            .await
            .map_err(DatabaseError::Sqlx)?;

        Ok(user_rows.into_iter().map(Self::from).collect())
    }

    /// Fetches all users with their NIP-65 relay URLs in a single query.
    ///
    /// This uses a LEFT JOIN to avoid the N+1 query problem of fetching
    /// each user's relays individually. Users with no NIP-65 relays are
    /// included with an empty relay URL list.
    pub(crate) async fn all_with_nip65_relay_urls(
        database: &Database,
    ) -> Result<Vec<(PublicKey, Vec<RelayUrl>)>, WhitenoiseError> {
        let relay_type_str = String::from(RelayType::Nip65);

        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT u.pubkey, r.url
             FROM users u
             LEFT JOIN user_relays ur ON u.id = ur.user_id AND ur.relay_type = ?
             LEFT JOIN relays r ON ur.relay_id = r.id
             ORDER BY u.pubkey",
        )
        .bind(&relay_type_str)
        .fetch_all(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        // Rows arrive sorted by pubkey, so we can group in a single pass
        // without a HashMap.
        let mut result: Vec<(PublicKey, Vec<RelayUrl>)> = Vec::new();
        let mut current_pubkey: Option<String> = None;

        for (pubkey_hex, relay_url) in rows {
            if current_pubkey.as_ref() != Some(&pubkey_hex) {
                let pk =
                    PublicKey::parse(&pubkey_hex).map_err(|e| WhitenoiseError::Other(e.into()))?;
                result.push((pk, Vec::new()));
                current_pubkey = Some(pubkey_hex);
            }
            if let Some(url_str) = relay_url
                && let Ok(url) = RelayUrl::parse(&url_str)
            {
                result.last_mut().unwrap().1.push(url);
            }
        }

        Ok(result)
    }

    /// Finds an existing user by public key or creates a new one if not found.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - A reference to the `PublicKey` to search for
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns a tuple containing the `User` and a boolean indicating if the user was newly created (true) or found (false).
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operations fail.
    pub(crate) async fn find_or_create_by_pubkey(
        pubkey: &PublicKey,
        database: &Database,
    ) -> Result<(User, bool), WhitenoiseError> {
        match User::find_by_pubkey(pubkey, database).await {
            Ok(user) => Ok((user, false)),
            Err(WhitenoiseError::UserNotFound) => {
                let mut user = User {
                    id: None,
                    pubkey: *pubkey,
                    metadata: Metadata::new(),
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                user = user.save(database).await?;
                Ok((user, true))
            }
            Err(e) => Err(e),
        }
    }

    /// Finds a user by their public key.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - A reference to the `PublicKey` to search for
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns the `User` associated with the provided public key on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError::UserNotFound`] if no user with the given public key exists.
    pub(crate) async fn find_by_pubkey(
        pubkey: &PublicKey,
        database: &Database,
    ) -> Result<User, WhitenoiseError> {
        let user_row = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE pubkey = ?")
            .bind(pubkey.to_hex().as_str())
            .fetch_one(&database.pool)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => WhitenoiseError::UserNotFound,
                other => WhitenoiseError::Database(DatabaseError::Sqlx(other)),
            })?;

        Ok(user_row.into())
    }

    /// Fetches multiple users by their public keys in a single query.
    ///
    /// # Arguments
    ///
    /// * `pubkeys` - A slice of `PublicKey` to search for
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns a `Vec<User>` containing all users found.
    /// Users that don't exist in the database are simply not included in the result.
    pub(crate) async fn find_by_pubkeys(
        pubkeys: &[PublicKey],
        database: &Database,
    ) -> Result<Vec<User>, WhitenoiseError> {
        if pubkeys.is_empty() {
            return Ok(Vec::new());
        }

        let pubkey_hexes: Vec<String> = pubkeys.iter().map(|pk| pk.to_hex()).collect();

        // Build dynamic query with correct number of placeholders
        let placeholders = "?,".repeat(pubkey_hexes.len());
        let placeholders = placeholders.trim_end_matches(',');

        let query = format!(
            "SELECT id, pubkey, metadata, created_at, updated_at
             FROM users
             WHERE pubkey IN ({})",
            placeholders
        );

        // Build and execute query with bindings
        let mut query_builder = sqlx::query_as::<_, UserRow>(&query);
        for hex in &pubkey_hexes {
            query_builder = query_builder.bind(hex);
        }

        let rows = query_builder
            .fetch_all(&database.pool)
            .await
            .map_err(DatabaseError::Sqlx)?;

        Ok(rows.into_iter().map(User::from).collect())
    }

    /// Gets all relays of a specific type associated with this user.
    ///
    /// # Arguments
    ///
    /// * `relay_type` - The type of relays to retrieve
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns a `Vec<Relay>` containing all relays of the specified type for this user.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database query fails.
    pub(crate) async fn relays(
        &self,
        relay_type: RelayType,
        database: &Database,
    ) -> Result<Vec<Relay>, WhitenoiseError> {
        let user_id = self.id.ok_or(WhitenoiseError::UserNotPersisted)?;
        let relay_type_str = String::from(relay_type);

        let relay_rows = sqlx::query_as::<_, RelayRow>(
            "SELECT r.id, r.url, r.created_at, r.updated_at
             FROM relays r
             INNER JOIN user_relays ur ON r.id = ur.relay_id
             WHERE ur.user_id = ? AND ur.relay_type = ?",
        )
        .bind(user_id)
        .bind(relay_type_str)
        .fetch_all(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)
        .map_err(WhitenoiseError::Database)?;

        let relays = relay_rows
            .into_iter()
            .map(|row| Relay {
                id: Some(row.id),
                url: row.url,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
            .collect();

        Ok(relays)
    }

    /// Saves this user to the database.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns the updated `User` with the database-assigned ID on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operation fails.
    pub(crate) async fn save(&self, database: &Database) -> Result<User, WhitenoiseError> {
        let mut tx = database.pool.begin().await.map_err(DatabaseError::Sqlx)?;

        // Use INSERT ON CONFLICT to handle both insert and update cases
        let current_time = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO users (pubkey, metadata, created_at, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(pubkey) DO UPDATE SET
               metadata = excluded.metadata,
               updated_at = ?",
        )
        .bind(self.pubkey.to_hex().as_str())
        .bind(serde_json::to_string(&self.metadata).map_err(DatabaseError::Serialization)?)
        .bind(self.created_at.timestamp_millis())
        .bind(self.updated_at.timestamp_millis())
        .bind(current_time)
        .execute(&mut *tx)
        .await
        .map_err(DatabaseError::Sqlx)
        .map_err(WhitenoiseError::Database)?;

        // Get the user by pubkey to return the updated record
        let updated_user = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE pubkey = ?")
            .bind(self.pubkey.to_hex().as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(DatabaseError::Sqlx)?;

        tx.commit().await.map_err(DatabaseError::Sqlx)?;

        Ok(updated_user.into())
    }

    /// Transaction-aware version of find_by_pubkey that accepts a database transaction.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - A reference to the `PublicKey` to search for
    /// * `tx` - A mutable reference to an active database transaction
    ///
    /// # Returns
    ///
    /// Returns the `User` associated with the provided public key on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError::UserNotFound`] if no user with the given public key exists.
    /// Returns other database errors if the query fails.
    pub(crate) async fn find_by_pubkey_tx<'a>(
        pubkey: &PublicKey,
        tx: &mut sqlx::Transaction<'a, sqlx::Sqlite>,
    ) -> Result<User, WhitenoiseError> {
        let user_row = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE pubkey = ?")
            .bind(pubkey.to_hex().as_str())
            .fetch_one(&mut **tx)
            .await
            .map_err(|err| match err {
                sqlx::Error::RowNotFound => WhitenoiseError::UserNotFound,
                _ => WhitenoiseError::Database(DatabaseError::Sqlx(err)),
            })?;

        Ok(user_row.into())
    }

    /// Transaction-aware version of save that accepts a database transaction.
    ///
    /// This method saves or updates the user within an existing transaction,
    /// avoiding nested transactions that could cause inconsistent state.
    ///
    /// # Arguments
    ///
    /// * `tx` - A mutable reference to an active database transaction
    ///
    /// # Returns
    ///
    /// Returns the saved/updated `User` on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operation fails.
    pub(crate) async fn save_tx<'a>(
        &self,
        tx: &mut sqlx::Transaction<'a, sqlx::Sqlite>,
    ) -> Result<User, WhitenoiseError> {
        // Use INSERT ON CONFLICT to handle both insert and update cases
        let current_time = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO users (pubkey, metadata, created_at, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(pubkey) DO UPDATE SET
               metadata = excluded.metadata,
               updated_at = ?",
        )
        .bind(self.pubkey.to_hex().as_str())
        .bind(serde_json::to_string(&self.metadata).map_err(DatabaseError::Serialization)?)
        .bind(self.created_at.timestamp_millis())
        .bind(self.updated_at.timestamp_millis())
        .bind(current_time)
        .execute(&mut **tx)
        .await
        .map_err(DatabaseError::Sqlx)
        .map_err(WhitenoiseError::Database)?;

        // Get the user by pubkey to return the updated record
        let updated_user = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE pubkey = ?")
            .bind(self.pubkey.to_hex().as_str())
            .fetch_one(&mut **tx)
            .await
            .map_err(DatabaseError::Sqlx)?;

        Ok(updated_user.into())
    }

    /// Transaction-aware version of find_or_create_by_pubkey that accepts a database transaction.
    ///
    /// This method finds an existing user by public key or creates a new one if not found,
    /// all within the provided transaction to maintain consistency.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - A reference to the `PublicKey` to search for
    /// * `tx` - A mutable reference to an active database transaction
    ///
    /// # Returns
    ///
    /// Returns a tuple containing the `User` and a boolean indicating if the user was newly created (true) or found (false).
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operations fail.
    pub(crate) async fn find_or_create_by_pubkey_tx<'a>(
        pubkey: &PublicKey,
        tx: &mut sqlx::Transaction<'a, sqlx::Sqlite>,
    ) -> Result<(User, bool), WhitenoiseError> {
        match User::find_by_pubkey_tx(pubkey, tx).await {
            Ok(user) => Ok((user, false)),
            Err(WhitenoiseError::UserNotFound) => {
                let mut user = User {
                    id: None,
                    pubkey: *pubkey,
                    metadata: Metadata::new(),
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                user = user.save_tx(tx).await?;
                Ok((user, true))
            }
            Err(e) => Err(e),
        }
    }

    /// Adds a relay association for this user.
    ///
    /// # Arguments
    ///
    /// * `relay` - A reference to the `Relay` to add
    /// * `relay_type` - The type of relay association
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operation fails.
    pub(crate) async fn add_relay(
        &self,
        relay: &Relay,
        relay_type: RelayType,
        database: &Database,
    ) -> Result<(), WhitenoiseError> {
        let user_id = self.id.ok_or(WhitenoiseError::UserNotPersisted)?;
        let relay_id = relay.id.expect("Relay should have ID after save");

        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(user_id, relay_id, relay_type) DO UPDATE SET updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(relay_id)
        .bind(String::from(relay_type))
        .bind(self.created_at.timestamp_millis())
        .bind(self.updated_at.timestamp_millis())
        .execute(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)
        .map_err(WhitenoiseError::Database)?;

        Ok(())
    }

    /// Adds multiple relay associations for this user in a batch operation.
    ///
    /// # Arguments
    ///
    /// * `relays` - A slice of `Relay` references to add
    /// * `relay_type` - The type of relay association for all relays
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operation fails.
    pub(crate) async fn add_relays(
        &self,
        relays: &[Relay],
        relay_type: RelayType,
        database: &Database,
    ) -> Result<(), WhitenoiseError> {
        if relays.is_empty() {
            return Ok(());
        }

        let user_id: i64 = self.id.ok_or(WhitenoiseError::UserNotPersisted)?;
        let relay_type_str = String::from(relay_type);
        let created_at = self.created_at.timestamp_millis();
        let updated_at = self.updated_at.timestamp_millis();

        // Build a single batch insert query for all relays
        let mut query_builder = sqlx::QueryBuilder::new(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) ",
        );

        query_builder.push_values(relays, |mut b, relay| {
            let relay_id = relay.id.expect("Relay should have ID after save");
            b.push_bind(user_id)
                .push_bind(relay_id)
                .push_bind(&relay_type_str)
                .push_bind(created_at)
                .push_bind(updated_at);
        });

        query_builder.push(" ON CONFLICT(user_id, relay_id, relay_type) DO UPDATE SET updated_at = excluded.updated_at");

        query_builder
            .build()
            .execute(&database.pool)
            .await
            .map_err(DatabaseError::Sqlx)
            .map_err(WhitenoiseError::Database)?;

        Ok(())
    }

    /// Removes a relay association for this user.
    ///
    /// # Arguments
    ///
    /// * `relay` - A reference to the `Relay` to remove
    /// * `relay_type` - The type of relay association to remove
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError::UserRelayNotFound`] if the relay association doesn't exist.
    /// Returns other [`WhitenoiseError`] variants if the database operation fails.
    pub(crate) async fn remove_relay(
        &self,
        relay: &Relay,
        relay_type: RelayType,
        database: &Database,
    ) -> Result<(), WhitenoiseError> {
        let user_id = self.id.ok_or(WhitenoiseError::UserNotPersisted)?;

        let result = sqlx::query(
            "DELETE FROM user_relays
             WHERE user_id = ?
             AND relay_id = (SELECT id FROM relays WHERE url = ?)
             AND relay_type = ?",
        )
        .bind(user_id)
        .bind(relay.url.to_string())
        .bind(String::from(relay_type))
        .execute(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)
        .map_err(WhitenoiseError::Database)?;

        if result.rows_affected() < 1 {
            Err(WhitenoiseError::UserRelayNotFound)
        } else {
            Ok(())
        }
    }

    /// Remove all relay associations for this user across all relay types.
    ///
    /// Used by [`Whitenoise::login_cancel`] to clean up relay associations that
    /// were written by [`try_discover_relay_lists`] before the user cancelled
    /// the login flow.  Without this, `user_relays` rows for the partial
    /// discovery would persist even after the account is deleted, causing stale
    /// data if the same pubkey logs in again later.
    pub(crate) async fn remove_all_relays(
        &self,
        database: &Database,
    ) -> Result<(), WhitenoiseError> {
        let user_id = self.id.ok_or(WhitenoiseError::UserNotPersisted)?;
        sqlx::query("DELETE FROM user_relays WHERE user_id = ?")
            .bind(user_id)
            .execute(&database.pool)
            .await
            .map_err(DatabaseError::Sqlx)
            .map_err(WhitenoiseError::Database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteRow;
    use sqlx::{FromRow, SqlitePool};

    // Helper function to create a mock SQLite row
    async fn setup_test_db() -> SqlitePool {
        let pool = SqlitePool::connect(":memory:").await.unwrap();

        // Create the users table with the new schema
        sqlx::query(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pubkey TEXT NOT NULL,
                metadata JSONB,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    #[tokio::test]
    async fn test_user_row_from_row_valid_data() {
        let pool = setup_test_db().await;

        // Create valid test data
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new()
            .name("Test User")
            .display_name("Test Display Name")
            .about("Test about section");
        let test_metadata_json = serde_json::to_string(&test_metadata).unwrap();
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        // Insert test data
        sqlx::query(
            "INSERT INTO users (pubkey, metadata, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(test_pubkey.to_hex())
        .bind(test_metadata_json)
        .bind(test_timestamp)
        .bind(test_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        // Test from_row implementation
        let row: SqliteRow = sqlx::query("SELECT * FROM users WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .fetch_one(&pool)
            .await
            .unwrap();

        let user_row = UserRow::from_row(&row).unwrap();

        assert_eq!(user_row.pubkey, test_pubkey);
        assert_eq!(user_row.metadata.name, test_metadata.name);
        assert_eq!(user_row.metadata.display_name, test_metadata.display_name);
        assert_eq!(user_row.metadata.about, test_metadata.about);
        assert_eq!(user_row.created_at.timestamp_millis(), test_timestamp);
        assert_eq!(user_row.updated_at.timestamp_millis(), test_timestamp);
    }

    #[tokio::test]
    async fn test_user_row_from_row_minimal_metadata() {
        let pool = setup_test_db().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new(); // Minimal metadata
        let test_metadata_json = serde_json::to_string(&test_metadata).unwrap();
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT INTO users (pubkey, metadata, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(test_pubkey.to_hex())
        .bind(test_metadata_json)
        .bind(test_timestamp)
        .bind(test_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM users WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .fetch_one(&pool)
            .await
            .unwrap();

        let user_row = UserRow::from_row(&row).unwrap();
        assert_eq!(user_row.pubkey, test_pubkey);
        assert_eq!(user_row.metadata.name, None);
    }

    #[tokio::test]
    async fn test_user_row_from_row_invalid_pubkey() {
        let pool = setup_test_db().await;

        let invalid_pubkey = "invalid_hex_key";
        let test_metadata = Metadata::new();
        let test_metadata_json = serde_json::to_string(&test_metadata).unwrap();
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT INTO users (pubkey, metadata, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(invalid_pubkey)
        .bind(test_metadata_json)
        .bind(test_timestamp)
        .bind(test_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM users WHERE pubkey = ?")
            .bind(invalid_pubkey)
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = UserRow::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "pubkey");
        } else {
            panic!("Expected ColumnDecode error for pubkey");
        }
    }

    #[tokio::test]
    async fn test_user_row_from_row_invalid_metadata_json() {
        let pool = setup_test_db().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let invalid_json = "{invalid_json}";
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT INTO users (pubkey, metadata, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(test_pubkey.to_hex())
        .bind(invalid_json)
        .bind(test_timestamp)
        .bind(test_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM users WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = UserRow::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "metadata");
        } else {
            panic!("Expected ColumnDecode error for metadata");
        }
    }

    #[tokio::test]
    async fn test_user_row_from_row_timestamp_edge_cases() {
        let pool = setup_test_db().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new();
        let test_metadata_json = serde_json::to_string(&test_metadata).unwrap();

        // Test with timestamp 0 (Unix epoch)
        sqlx::query(
            "INSERT INTO users (pubkey, metadata, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(test_pubkey.to_hex())
        .bind(&test_metadata_json)
        .bind(0i64)
        .bind(0i64)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM users WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .fetch_one(&pool)
            .await
            .unwrap();

        let user_row = UserRow::from_row(&row).unwrap();
        assert_eq!(user_row.created_at.timestamp_millis(), 0);
        assert_eq!(user_row.updated_at.timestamp_millis(), 0);
    }

    #[tokio::test]
    async fn test_user_row_from_row_invalid_timestamps() {
        let pool = setup_test_db().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new();
        let test_metadata_json = serde_json::to_string(&test_metadata).unwrap();

        // Test with invalid timestamp that's out of range for DateTime
        let invalid_timestamp = i64::MAX; // This will be too large for DateTime conversion

        sqlx::query(
            "INSERT INTO users (pubkey, metadata, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(test_pubkey.to_hex())
        .bind(&test_metadata_json)
        .bind(invalid_timestamp)
        .bind(0i64) // Valid timestamp for updated_at
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM users WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = UserRow::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "created_at");
        } else {
            panic!("Expected ColumnDecode error for created_at timestamp");
        }

        // Clean up and test invalid updated_at timestamp
        sqlx::query("DELETE FROM users WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO users (pubkey, metadata, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(test_pubkey.to_hex())
        .bind(&test_metadata_json)
        .bind(0i64) // Valid timestamp for created_at
        .bind(invalid_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM users WHERE pubkey = ?")
            .bind(test_pubkey.to_hex())
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = UserRow::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "updated_at");
        } else {
            panic!("Expected ColumnDecode error for updated_at timestamp");
        }
    }

    #[tokio::test]
    async fn test_all_users() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let users = User::all(&whitenoise.database).await.unwrap();
        assert!(users.is_empty());

        let test_pubkey1 = nostr_sdk::Keys::generate().public_key();
        let test_pubkey2 = nostr_sdk::Keys::generate().public_key();

        let user1 = User {
            id: None,
            pubkey: test_pubkey1,
            metadata: Metadata::new().name("User One"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let user2 = User {
            id: None,
            pubkey: test_pubkey2,
            metadata: Metadata::new().name("User Two"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let saved1 = user1.save(&whitenoise.database).await.unwrap();
        let saved2 = user2.save(&whitenoise.database).await.unwrap();

        let all_users = User::all(&whitenoise.database).await.unwrap();
        assert_eq!(all_users.len(), 2);

        let pubkeys: Vec<_> = all_users.iter().map(|u| u.pubkey).collect();
        assert!(pubkeys.contains(&saved1.pubkey));
        assert!(pubkeys.contains(&saved2.pubkey));
    }

    #[tokio::test]
    async fn test_save_success() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new()
            .name("Test User")
            .display_name("Test Display Name")
            .about("Test about section");
        let test_created_at = chrono::Utc::now();
        let test_updated_at = chrono::Utc::now();

        // Test save
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata.clone(),
            created_at: test_created_at,
            updated_at: test_updated_at,
        };
        let result = user.save(&whitenoise.database).await;
        assert!(result.is_ok());

        // Test that we can load it back (this verifies it was saved correctly)
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database).await;
        assert!(loaded_user.is_ok());

        let loaded = loaded_user.unwrap();
        assert_eq!(loaded.pubkey, test_pubkey);
        assert_eq!(loaded.metadata.name, test_metadata.name);
        assert_eq!(loaded.metadata.display_name, test_metadata.display_name);
        assert_eq!(loaded.metadata.about, test_metadata.about);
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
    async fn test_save_with_minimal_metadata() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new(); // Minimal metadata
        let test_created_at = chrono::Utc::now();
        let test_updated_at = chrono::Utc::now();

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata.clone(),
            created_at: test_created_at,
            updated_at: test_updated_at,
        };

        let result = user.save(&whitenoise.database).await;
        assert!(result.is_ok());

        // Verify it was saved correctly by loading it back
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database).await;
        assert!(loaded_user.is_ok());

        let loaded = loaded_user.unwrap();
        assert_eq!(loaded.metadata.name, None);
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
    async fn test_load_user_not_found() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Try to load a non-existent user
        let non_existent_pubkey = nostr_sdk::Keys::generate().public_key();
        let result = User::find_by_pubkey(&non_existent_pubkey, &whitenoise.database).await;

        assert!(result.is_err());
        if let Err(WhitenoiseError::UserNotFound) = result {
            // Expected error
        } else {
            panic!("Expected UserNotFound error");
        }
    }

    #[tokio::test]
    async fn test_find_by_pubkeys_empty_input() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let result = User::find_by_pubkeys(&[], &whitenoise.database)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_find_by_pubkeys_single_existing() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new().name("Test User"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        let result = User::find_by_pubkeys(&[test_pubkey], &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].pubkey, test_pubkey);
        assert_eq!(result[0].metadata.name, Some("Test User".to_string()));
    }

    #[tokio::test]
    async fn test_find_by_pubkeys_multiple_existing() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pubkey1 = nostr_sdk::Keys::generate().public_key();
        let pubkey2 = nostr_sdk::Keys::generate().public_key();
        let pubkey3 = nostr_sdk::Keys::generate().public_key();

        for (i, pubkey) in [pubkey1, pubkey2, pubkey3].iter().enumerate() {
            let user = User {
                id: None,
                pubkey: *pubkey,
                metadata: Metadata::new().name(format!("User {}", i + 1)),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            user.save(&whitenoise.database).await.unwrap();
        }

        let result = User::find_by_pubkeys(&[pubkey1, pubkey2, pubkey3], &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(result.len(), 3);

        let pubkeys: Vec<_> = result.iter().map(|u| u.pubkey).collect();
        assert!(pubkeys.contains(&pubkey1));
        assert!(pubkeys.contains(&pubkey2));
        assert!(pubkeys.contains(&pubkey3));
    }

    #[tokio::test]
    async fn test_find_by_pubkeys_mixed_existing_and_missing() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let existing_pubkey = nostr_sdk::Keys::generate().public_key();
        let missing_pubkey = nostr_sdk::Keys::generate().public_key();

        let user = User {
            id: None,
            pubkey: existing_pubkey,
            metadata: Metadata::new().name("Existing User"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        let result =
            User::find_by_pubkeys(&[existing_pubkey, missing_pubkey], &whitenoise.database)
                .await
                .unwrap();

        // Should only contain the existing user
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].pubkey, existing_pubkey);
    }

    #[tokio::test]
    async fn test_find_by_pubkeys_none_exist() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pubkey1 = nostr_sdk::Keys::generate().public_key();
        let pubkey2 = nostr_sdk::Keys::generate().public_key();

        let result = User::find_by_pubkeys(&[pubkey1, pubkey2], &whitenoise.database)
            .await
            .unwrap();

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_find_by_pubkeys_preserves_metadata() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new()
            .name("Full Name")
            .display_name("Display Name")
            .about("About text")
            .nip05("user@example.com");

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata.clone(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        let result = User::find_by_pubkeys(&[test_pubkey], &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let loaded_user = &result[0];
        assert_eq!(loaded_user.metadata.name, test_metadata.name);
        assert_eq!(
            loaded_user.metadata.display_name,
            test_metadata.display_name
        );
        assert_eq!(loaded_user.metadata.about, test_metadata.about);
        assert_eq!(loaded_user.metadata.nip05, test_metadata.nip05);
    }

    #[tokio::test]
    async fn test_save_and_load_user_roundtrip() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user with comprehensive metadata
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new()
            .name("Complete Test User")
            .display_name("Complete Display Name")
            .about("Complete test about section")
            .nip05("test@example.com")
            .lud06("lnurl1dp68gurn8ghj7urp0v4kxvern9eehqurfdcsk6arpdd5kuemmduhxcmmrdaehgu3wd3skuep0dejhctnwda3kxvd09eszuekd0v8rqnrpwcxk7trj0ae8gmmwv9unx2txvg6xqmwpwejkcmmfd9c");
        let test_created_at = chrono::Utc::now();
        let test_updated_at = chrono::Utc::now();

        let original_user = User {
            id: Some(1),
            pubkey: test_pubkey,
            metadata: test_metadata.clone(),
            created_at: test_created_at,
            updated_at: test_updated_at,
        };

        // Save the user
        let save_result = original_user.save(&whitenoise.database).await;
        assert!(save_result.is_ok());

        // Load the user back
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database).await;
        assert!(loaded_user.is_ok());

        let user = loaded_user.unwrap();

        // Verify all fields match (except id which is set by database)
        assert_eq!(user.pubkey, original_user.pubkey);
        assert_eq!(user.metadata.name, original_user.metadata.name);
        assert_eq!(
            user.metadata.display_name,
            original_user.metadata.display_name
        );
        assert_eq!(user.metadata.about, original_user.metadata.about);
        assert_eq!(user.metadata.picture, original_user.metadata.picture);
        assert_eq!(user.metadata.banner, original_user.metadata.banner);
        assert_eq!(user.metadata.nip05, original_user.metadata.nip05);
        assert_eq!(user.metadata.lud06, original_user.metadata.lud06);
        assert_eq!(
            user.created_at.timestamp_millis(),
            original_user.created_at.timestamp_millis()
        );
        assert_eq!(
            user.updated_at.timestamp_millis(),
            original_user.updated_at.timestamp_millis()
        );
    }

    #[tokio::test]
    async fn test_save_with_complex_metadata() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Test with various metadata combinations
        let test_cases = [
            (
                "User with all fields",
                Metadata::new()
                    .name("Full User")
                    .display_name("Full Display")
                    .picture(nostr_sdk::types::url::Url::parse("https://example.com/avatar.jpg").unwrap())
                    .banner(nostr_sdk::types::url::Url::parse("https://example.com/banner.jpg").unwrap())
                    .about("Full about")
                    .nip05("full@example.com")
                    .lud06("lnurl1dp68gurn8ghj7urp0v4kxvern9eehqurfdcsk6arpdd5kuemmduhxcmmrdaehgu3wd3skuep0dejhctnwda3kxvd09eszuekd0v8rqnrpwcxk7trj0ae8gmmwv9unx2txvg6xqmwpwejkcmmfd9c"),
            ),
            (
                "User with only name",
                Metadata::new().name("Name Only"),
            ),
            (
                "User with name and about",
                Metadata::new().name("Named User").about("Has about section"),
            ),
            (
                "User with empty metadata",
                Metadata::new(),
            ),
        ];

        let test_timestamp = chrono::Utc::now();

        for (description, metadata) in test_cases {
            let test_pubkey = nostr_sdk::Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: metadata.clone(),
                created_at: test_timestamp,
                updated_at: test_timestamp,
            };

            // Save the user
            let save_result = user.save(&whitenoise.database).await;
            assert!(save_result.is_ok(), "Failed to save {}", description);

            // Load the user back
            let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database).await;
            assert!(loaded_user.is_ok(), "Failed to load {}", description);

            let loaded = loaded_user.unwrap();
            assert_eq!(
                loaded.metadata.name, metadata.name,
                "{}: name mismatch",
                description
            );
            assert_eq!(
                loaded.metadata.display_name, metadata.display_name,
                "{}: display_name mismatch",
                description
            );
            assert_eq!(
                loaded.metadata.about, metadata.about,
                "{}: about mismatch",
                description
            );
            assert_eq!(
                loaded.metadata.picture, metadata.picture,
                "{}: picture mismatch",
                description
            );
            assert_eq!(
                loaded.metadata.banner, metadata.banner,
                "{}: banner mismatch",
                description
            );
            assert_eq!(
                loaded.metadata.nip05, metadata.nip05,
                "{}: nip05 mismatch",
                description
            );
            assert_eq!(
                loaded.metadata.lud06, metadata.lud06,
                "{}: lud06 mismatch",
                description
            );
        }
    }

    #[tokio::test]
    async fn test_relays_success() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new().name("Test User");
        let test_timestamp = chrono::Utc::now();

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata.clone(),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        // Save the user first
        let save_result = user.save(&whitenoise.database).await;
        assert!(save_result.is_ok());

        // Load the user to get the actual database ID
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Create test relays
        let relay1_url = nostr_sdk::RelayUrl::parse("wss://relay1.example.com").unwrap();
        let relay2_url = nostr_sdk::RelayUrl::parse("wss://relay2.example.com").unwrap();
        let relay3_url = nostr_sdk::RelayUrl::parse("wss://relay3.example.com").unwrap();

        let relay1 = Relay {
            id: Some(1),
            url: relay1_url.clone(),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };
        let relay2 = Relay {
            id: Some(2),
            url: relay2_url.clone(),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };
        let relay3 = Relay {
            id: Some(3),
            url: relay3_url.clone(),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        // Save relays to database
        relay1.save(&whitenoise.database).await.unwrap();
        relay2.save(&whitenoise.database).await.unwrap();
        relay3.save(&whitenoise.database).await.unwrap();

        // Insert into user_relays table
        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(loaded_user.id)
        .bind(1) // relay1
        .bind("nip65")
        .bind(test_timestamp.timestamp_millis())
        .bind(test_timestamp.timestamp_millis())
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(loaded_user.id)
        .bind(2) // relay2
        .bind("nip65")
        .bind(test_timestamp.timestamp_millis())
        .bind(test_timestamp.timestamp_millis())
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(loaded_user.id)
        .bind(3) // relay3
        .bind("inbox")
        .bind(test_timestamp.timestamp_millis())
        .bind(test_timestamp.timestamp_millis())
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        // Test loading nostr relays
        let nostr_relays = loaded_user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(nostr_relays.len(), 2);
        let relay_urls: Vec<_> = nostr_relays.iter().map(|r| &r.url).collect();
        assert!(relay_urls.contains(&&relay1_url));
        assert!(relay_urls.contains(&&relay2_url));
        assert!(!relay_urls.contains(&&relay3_url));

        // Test loading inbox relays
        let inbox_relays = loaded_user
            .relays(RelayType::Inbox, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(inbox_relays.len(), 1);
        assert_eq!(inbox_relays[0].url, relay3_url);

        // Test loading key package relays (should be empty)
        let key_package_relays = loaded_user
            .relays(RelayType::KeyPackage, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(key_package_relays.len(), 0);
    }

    #[tokio::test]
    async fn test_relays_empty_result() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new().name("Test User");
        let test_timestamp = chrono::Utc::now();

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata.clone(),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        // Save the user first
        user.save(&whitenoise.database).await.unwrap();
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Test loading relays when none exist
        let result = loaded_user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn test_relays_multiple_relay_types() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new().name("Test User");
        let test_timestamp = chrono::Utc::now();

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata.clone(),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        user.save(&whitenoise.database).await.unwrap();
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Create and save a test relay
        let relay_url = nostr_sdk::RelayUrl::parse("wss://multi.example.com").unwrap();
        let relay = Relay {
            id: Some(1),
            url: relay_url.clone(),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };
        relay.save(&whitenoise.database).await.unwrap();

        // Add the same relay for different types
        for relay_type in ["nip65", "inbox", "key_package"] {
            sqlx::query(
                "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(loaded_user.id)
            .bind(1)
            .bind(relay_type)
            .bind(test_timestamp.timestamp_millis())
            .bind(test_timestamp.timestamp_millis())
            .execute(&whitenoise.database.pool)
            .await
            .unwrap();
        }

        // Test each relay type returns the same relay
        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            let relays = loaded_user
                .relays(relay_type, &whitenoise.database)
                .await
                .unwrap();

            assert_eq!(relays.len(), 1);
            assert_eq!(relays[0].url, relay_url);
        }
    }

    #[tokio::test]
    async fn test_relays_different_users() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create two test users
        let user1_pubkey = nostr_sdk::Keys::generate().public_key();
        let user2_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_timestamp = chrono::Utc::now();

        let user1 = User {
            id: None,
            pubkey: user1_pubkey,
            metadata: Metadata::new().name("User 1"),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        let user2 = User {
            id: None,
            pubkey: user2_pubkey,
            metadata: Metadata::new().name("User 2"),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        user1.save(&whitenoise.database).await.unwrap();
        user2.save(&whitenoise.database).await.unwrap();

        let loaded_user1 = User::find_by_pubkey(&user1_pubkey, &whitenoise.database)
            .await
            .unwrap();
        let loaded_user2 = User::find_by_pubkey(&user2_pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Create test relays
        let relay1_url = nostr_sdk::RelayUrl::parse("wss://user1.example.com").unwrap();
        let relay2_url = nostr_sdk::RelayUrl::parse("wss://user2.example.com").unwrap();

        let relay1 = Relay {
            id: Some(1),
            url: relay1_url.clone(),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };
        let relay2 = Relay {
            id: Some(2),
            url: relay2_url.clone(),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        relay1.save(&whitenoise.database).await.unwrap();
        relay2.save(&whitenoise.database).await.unwrap();

        // Associate relay1 with user1 and relay2 with user2
        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(loaded_user1.id)
        .bind(1)
        .bind("nip65")
        .bind(test_timestamp.timestamp_millis())
        .bind(test_timestamp.timestamp_millis())
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(loaded_user2.id)
        .bind(2)
        .bind("nip65")
        .bind(test_timestamp.timestamp_millis())
        .bind(test_timestamp.timestamp_millis())
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        // Test that each user gets only their own relays
        let user1_relays = loaded_user1
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        let user2_relays = loaded_user2
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(user1_relays.len(), 1);
        assert_eq!(user1_relays[0].url, relay1_url);

        assert_eq!(user2_relays.len(), 1);
        assert_eq!(user2_relays[0].url, relay2_url);
    }

    #[tokio::test]
    async fn test_add_relay_success() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new().name("Test User");
        let test_timestamp = chrono::Utc::now();

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata,
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        user.save(&whitenoise.database).await.unwrap();
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Test adding a relay when it already exists in the database
        let relay_url = nostr_sdk::RelayUrl::parse("wss://test-add.example.com").unwrap();

        // Pre-create the relay in the database
        let existing_relay = Relay::new(&relay_url);
        let saved_relay = existing_relay.save(&whitenoise.database).await.unwrap();

        // Add relay association - should work with existing relay
        let result = loaded_user
            .add_relay(&saved_relay, RelayType::Nip65, &whitenoise.database)
            .await;
        assert!(result.is_ok());

        // Verify the relay was associated
        let user_relays = loaded_user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(user_relays.len(), 1);
        assert_eq!(user_relays[0].url, relay_url);
    }

    #[tokio::test]
    async fn test_add_relay_creates_new_relay() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new().name("Test User");
        let test_timestamp = chrono::Utc::now();

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata,
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        user.save(&whitenoise.database).await.unwrap();
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Test adding a relay that doesn't exist in the database yet
        let new_relay_url = nostr_sdk::RelayUrl::parse("wss://new-relay.example.com").unwrap();

        // Verify relay doesn't exist yet
        let find_result = Relay::find_by_url(&new_relay_url, &whitenoise.database).await;
        assert!(find_result.is_err());

        // Create the relay (this is what find_or_create_relay would do)
        let new_relay = whitenoise
            .find_or_create_relay_by_url(&new_relay_url)
            .await
            .unwrap();

        // Add relay association
        let result = loaded_user
            .add_relay(&new_relay, RelayType::Inbox, &whitenoise.database)
            .await;
        assert!(result.is_ok());

        // Verify the relay was associated
        let user_relays = loaded_user
            .relays(RelayType::Inbox, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(user_relays.len(), 1);
        assert_eq!(user_relays[0].url, new_relay_url);

        // Verify relay exists in database (since we created it)
        let find_result = Relay::find_by_url(&new_relay_url, &whitenoise.database).await;
        assert!(find_result.is_ok());
    }

    #[tokio::test]
    async fn test_remove_relay_success() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new().name("Test User");
        let test_timestamp = chrono::Utc::now();

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata,
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        user.save(&whitenoise.database).await.unwrap();
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Add a relay first
        let relay_url = nostr_sdk::RelayUrl::parse("wss://test-remove.example.com").unwrap();
        let relay = whitenoise
            .find_or_create_relay_by_url(&relay_url)
            .await
            .unwrap();
        loaded_user
            .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        // Verify relay was added
        let user_relays = loaded_user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(user_relays.len(), 1);

        // Remove the relay
        let result = loaded_user
            .remove_relay(&relay, RelayType::Nip65, &whitenoise.database)
            .await;
        assert!(result.is_ok());

        // Verify relay was removed
        let user_relays = loaded_user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(user_relays.len(), 0);
    }

    #[tokio::test]
    async fn test_remove_relay_not_in_database() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new().name("Test User");
        let test_timestamp = chrono::Utc::now();

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata,
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        user.save(&whitenoise.database).await.unwrap();
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Try to remove a relay that doesn't exist in the database
        let non_existent_url =
            nostr_sdk::RelayUrl::parse("wss://non-existent.example.com").unwrap();
        let non_existent_relay = Relay::new(&non_existent_url);
        let result = loaded_user
            .remove_relay(&non_existent_relay, RelayType::Nip65, &whitenoise.database)
            .await;

        assert!(result.is_err());
        if let Err(WhitenoiseError::UserRelayNotFound) = result {
            // Expected - relay doesn't exist in database, so no association can be removed
        } else {
            panic!("Expected UserRelayNotFound error, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_remove_relay_not_associated() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new().name("Test User");
        let test_timestamp = chrono::Utc::now();

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata,
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        user.save(&whitenoise.database).await.unwrap();
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Create a relay in the database but don't associate it with the user
        let relay_url = nostr_sdk::RelayUrl::parse("wss://unassociated.example.com").unwrap();
        let relay = Relay::new(&relay_url);
        let saved_relay = relay.save(&whitenoise.database).await.unwrap();

        // Try to remove the relay - it exists in database but not associated with user
        let result = loaded_user
            .remove_relay(&saved_relay, RelayType::Nip65, &whitenoise.database)
            .await;

        assert!(result.is_err());
        if let Err(WhitenoiseError::UserRelayNotFound) = result {
            // Expected - relay exists but no association with user
        } else {
            panic!("Expected UserRelayNotFound error, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_relays_relay_properties() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create test user
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let test_metadata = Metadata::new().name("Test User");
        let test_timestamp = chrono::Utc::now();

        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: test_metadata.clone(),
            created_at: test_timestamp,
            updated_at: test_timestamp,
        };

        user.save(&whitenoise.database).await.unwrap();
        let loaded_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();

        // Create test relay with specific timestamps
        let relay_url = nostr_sdk::RelayUrl::parse("wss://properties.example.com").unwrap();
        let relay_created_at = chrono::Utc::now() - chrono::Duration::hours(1);
        let relay_updated_at = chrono::Utc::now() - chrono::Duration::minutes(30);

        let relay = Relay {
            id: Some(1),
            url: relay_url.clone(),
            created_at: relay_created_at,
            updated_at: relay_updated_at,
        };

        relay.save(&whitenoise.database).await.unwrap();

        // Associate with user
        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(loaded_user.id)
        .bind(1)
        .bind("nip65")
        .bind(test_timestamp.timestamp_millis())
        .bind(test_timestamp.timestamp_millis())
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        // Load relays and verify all properties
        let relays = loaded_user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        assert_eq!(relays.len(), 1);
        let loaded_relay = &relays[0];

        assert_eq!(loaded_relay.url, relay_url);
        assert_eq!(
            loaded_relay.created_at.timestamp_millis(),
            relay_created_at.timestamp_millis()
        );
        assert_eq!(
            loaded_relay.updated_at.timestamp_millis(),
            relay_updated_at.timestamp_millis()
        );
        // ID should be set by database
        assert!(loaded_relay.id.is_some());
    }
}
