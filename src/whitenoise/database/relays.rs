use chrono::{DateTime, Utc};
use nostr_sdk::RelayUrl;

use super::{
    Database, DatabaseError,
    utils::{normalize_relay_url, parse_timestamp},
};
use crate::{WhitenoiseError, perf_instrument, whitenoise::relays::Relay};

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub(crate) struct RelayRow {
    // id is the primary key
    pub id: i64,
    // url is the URL of the relay
    pub url: RelayUrl,
    // created_at is the timestamp of the relay creation
    pub created_at: DateTime<Utc>,
    // updated_at is the timestamp of the last update
    pub updated_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for RelayRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let url_str: String = row.try_get("url")?;

        // Parse url from string
        let url = RelayUrl::parse(&url_str).map_err(|e| sqlx::Error::ColumnDecode {
            index: "url".to_string(),
            source: Box::new(e),
        })?;

        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(RelayRow {
            id,
            url,
            created_at,
            updated_at,
        })
    }
}

impl From<RelayRow> for Relay {
    fn from(val: RelayRow) -> Self {
        Relay {
            id: Some(val.id),
            url: val.url,
            created_at: val.created_at,
            updated_at: val.updated_at,
        }
    }
}

impl Relay {
    /// Finds a relay by its URL.
    ///
    /// # Arguments
    ///
    /// * `url` - A reference to the `RelayUrl` to search for
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns the `Relay` associated with the provided URL on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError::RelayNotFound`] if no relay with the given URL exists.
    #[perf_instrument("db::relays")]
    pub(crate) async fn find_by_url(
        url: &RelayUrl,
        database: &Database,
    ) -> Result<Relay, WhitenoiseError> {
        let normalized_url = normalize_relay_url(url);
        let relay_row = sqlx::query_as::<_, RelayRow>("SELECT * FROM relays WHERE url = ?")
            .bind(normalized_url)
            .fetch_one(&database.pool)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => WhitenoiseError::RelayNotFound,
                other => WhitenoiseError::Database(DatabaseError::Sqlx(other)),
            })?;

        Ok(Relay {
            id: Some(relay_row.id),
            url: relay_row.url,
            created_at: relay_row.created_at,
            updated_at: relay_row.updated_at,
        })
    }

    #[perf_instrument("db::relays")]
    pub(crate) async fn find_or_create_by_url(
        url: &RelayUrl,
        database: &Database,
    ) -> Result<Relay, WhitenoiseError> {
        match Relay::find_by_url(url, database).await {
            Ok(relay) => Ok(relay),
            Err(WhitenoiseError::RelayNotFound) => {
                let relay = Relay::new(url);
                let new_relay = relay.save(database).await?;
                Ok(new_relay)
            }
            Err(e) => Err(e),
        }
    }

    /// Saves this relay to the database.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns the updated `Relay` with the database-assigned ID on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operation fails.
    #[perf_instrument("db::relays")]
    pub(crate) async fn save(&self, database: &Database) -> Result<Relay, WhitenoiseError> {
        let mut tx = database.pool.begin().await.map_err(DatabaseError::Sqlx)?;
        let normalized_url = normalize_relay_url(&self.url);

        sqlx::query(
            "INSERT INTO relays (url, created_at, updated_at) VALUES (?, ?, ?) ON CONFLICT(url) DO UPDATE SET updated_at = ?",
        )
        .bind(&normalized_url)
        .bind(self.created_at.timestamp_millis())
        .bind(self.updated_at.timestamp_millis())
        .bind(Utc::now().timestamp_millis())
        .execute(&mut *tx)
        .await
        .map_err(DatabaseError::Sqlx)?;

        let inserted_relay = sqlx::query_as::<_, RelayRow>("SELECT * FROM relays WHERE url = ?")
            .bind(&normalized_url)
            .fetch_one(&mut *tx)
            .await
            .map_err(DatabaseError::Sqlx)?;

        tx.commit().await.map_err(DatabaseError::Sqlx)?;

        Ok(inserted_relay.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteRow;
    use sqlx::{FromRow, SqlitePool};
    use std::path::PathBuf;

    async fn setup_test_db() -> SqlitePool {
        let pool = SqlitePool::connect(":memory:").await.unwrap();

        // Create the relays table
        sqlx::query(
            "CREATE TABLE relays (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                url TEXT NOT NULL UNIQUE,
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
    async fn test_relay_new_row_from_row_valid_data() {
        let pool = setup_test_db().await;

        let test_url_str = "wss://relay.damus.io";
        let test_url = RelayUrl::parse(test_url_str).unwrap();
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        sqlx::query("INSERT INTO relays (url, created_at, updated_at) VALUES (?, ?, ?)")
            .bind(test_url_str)
            .bind(test_timestamp)
            .bind(test_timestamp)
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM relays WHERE url = ?")
            .bind(test_url_str)
            .fetch_one(&pool)
            .await
            .unwrap();

        let relay_row = RelayRow::from_row(&row).unwrap();

        assert_eq!(relay_row.url, test_url);
        assert_eq!(relay_row.created_at.timestamp_millis(), test_timestamp);
        assert_eq!(relay_row.updated_at.timestamp_millis(), test_timestamp);
    }

    #[tokio::test]
    async fn test_relay_new_row_from_row_various_valid_urls() {
        let pool = setup_test_db().await;

        let test_urls = vec![
            "wss://relay.damus.io",
            "wss://nos.lol",
            "wss://relay.snort.social",
            "wss://relay.nostr.band",
            "ws://localhost:8080", // Non-secure websocket for testing
        ];

        let test_timestamp = chrono::Utc::now().timestamp_millis();

        for url_str in test_urls {
            // Insert the URL
            sqlx::query("INSERT INTO relays (url, created_at, updated_at) VALUES (?, ?, ?)")
                .bind(url_str)
                .bind(test_timestamp)
                .bind(test_timestamp)
                .execute(&pool)
                .await
                .unwrap();

            // Fetch and test
            let row: SqliteRow = sqlx::query("SELECT * FROM relays WHERE url = ?")
                .bind(url_str)
                .fetch_one(&pool)
                .await
                .unwrap();

            let relay_row = RelayRow::from_row(&row).unwrap();
            let expected_url = RelayUrl::parse(url_str).unwrap();
            assert_eq!(relay_row.url, expected_url);

            // Clean up
            sqlx::query("DELETE FROM relays WHERE url = ?")
                .bind(url_str)
                .execute(&pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_relay_new_row_from_row_invalid_url() {
        let pool = setup_test_db().await;

        let invalid_urls = vec![
            "not_a_url",
            "http://invalid_for_relay.com", // HTTP instead of WS/WSS
            "ftp://not.websocket.com",
            "",
            "just some text",
            "wss://", // Incomplete URL
        ];

        let test_timestamp = chrono::Utc::now().timestamp_millis();

        for invalid_url in invalid_urls {
            // Insert invalid URL (SQLite will accept it)
            sqlx::query("INSERT INTO relays (url, created_at, updated_at) VALUES (?, ?, ?)")
                .bind(invalid_url)
                .bind(test_timestamp)
                .bind(test_timestamp)
                .execute(&pool)
                .await
                .unwrap();

            // Try to parse it with from_row - should fail
            let row: SqliteRow = sqlx::query("SELECT * FROM relays WHERE url = ?")
                .bind(invalid_url)
                .fetch_one(&pool)
                .await
                .unwrap();

            let result = RelayRow::from_row(&row);
            assert!(
                result.is_err(),
                "Expected error for invalid URL: {}",
                invalid_url
            );

            if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
                assert_eq!(index, "url");
            } else {
                panic!("Expected ColumnDecode error for url, got: {:?}", result);
            }

            // Clean up
            sqlx::query("DELETE FROM relays WHERE url = ?")
                .bind(invalid_url)
                .execute(&pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_relay_new_row_from_row_timestamp_edge_cases() {
        let pool = setup_test_db().await;

        let test_url = "wss://relay.damus.io";

        // Test with timestamp 0 (Unix epoch)
        sqlx::query("INSERT INTO relays (url, created_at, updated_at) VALUES (?, ?, ?)")
            .bind(test_url)
            .bind(0i64)
            .bind(0i64)
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM relays WHERE created_at = 0")
            .fetch_one(&pool)
            .await
            .unwrap();

        let relay_row = RelayRow::from_row(&row).unwrap();
        assert_eq!(relay_row.created_at.timestamp_millis(), 0);
        assert_eq!(relay_row.updated_at.timestamp_millis(), 0);

        // Clean up
        sqlx::query("DELETE FROM relays WHERE created_at = 0")
            .execute(&pool)
            .await
            .unwrap();

        // Test with future timestamp
        let future_timestamp =
            (chrono::Utc::now() + chrono::Duration::days(365)).timestamp_millis();
        sqlx::query("INSERT INTO relays (url, created_at, updated_at) VALUES (?, ?, ?)")
            .bind(test_url)
            .bind(future_timestamp)
            .bind(future_timestamp)
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM relays WHERE created_at = ?")
            .bind(future_timestamp)
            .fetch_one(&pool)
            .await
            .unwrap();

        let relay_row = RelayRow::from_row(&row).unwrap();
        assert_eq!(relay_row.created_at.timestamp_millis(), future_timestamp);
        assert_eq!(relay_row.updated_at.timestamp_millis(), future_timestamp);
    }

    #[tokio::test]
    async fn test_relay_new_row_from_row_url_with_port() {
        let pool = setup_test_db().await;

        let test_url_str = "wss://relay.example.com:8443";
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        sqlx::query("INSERT INTO relays (url, created_at, updated_at) VALUES (?, ?, ?)")
            .bind(test_url_str)
            .bind(test_timestamp)
            .bind(test_timestamp)
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM relays WHERE url = ?")
            .bind(test_url_str)
            .fetch_one(&pool)
            .await
            .unwrap();

        let relay_row = RelayRow::from_row(&row).unwrap();
        let expected_url = RelayUrl::parse(test_url_str).unwrap();
        assert_eq!(relay_row.url, expected_url);
    }

    #[tokio::test]
    async fn test_relay_new_row_from_row_url_with_path() {
        let pool = setup_test_db().await;

        let test_url_str = "wss://relay.example.com/nostr";
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        sqlx::query("INSERT INTO relays (url, created_at, updated_at) VALUES (?, ?, ?)")
            .bind(test_url_str)
            .bind(test_timestamp)
            .bind(test_timestamp)
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM relays WHERE url = ?")
            .bind(test_url_str)
            .fetch_one(&pool)
            .await
            .unwrap();

        let relay_row = RelayRow::from_row(&row).unwrap();
        let expected_url = RelayUrl::parse(test_url_str).unwrap();
        assert_eq!(relay_row.url, expected_url);
    }

    #[tokio::test]
    async fn test_relay_save_insert_and_update() {
        use crate::whitenoise::database::Database;
        use crate::whitenoise::relays::Relay;

        let pool = setup_test_db().await;
        let database = Database {
            pool,
            path: PathBuf::from(":memory:"),
            last_connected: std::time::SystemTime::now(),
        };

        let test_url = RelayUrl::parse("wss://relay.save.test").unwrap();

        let saved_relay1 = Relay::new(&test_url).save(&database).await.unwrap();
        let first_id = saved_relay1.id.unwrap();

        let saved_relay2 = Relay::new(&test_url).save(&database).await.unwrap();
        let second_id = saved_relay2.id.unwrap();

        assert_eq!(first_id, second_id);
        assert!(saved_relay2.updated_at >= saved_relay1.updated_at);
    }

    #[tokio::test]
    async fn test_normalize_relay_url() {
        // Test root path normalization (should remove trailing slash)
        let url_with_slash = RelayUrl::parse("wss://relay.com/").unwrap();
        let url_without_slash = RelayUrl::parse("wss://relay.com").unwrap();

        assert_eq!(normalize_relay_url(&url_with_slash), "wss://relay.com");
        assert_eq!(normalize_relay_url(&url_without_slash), "wss://relay.com");

        // Test localhost variants
        let localhost_with_slash = RelayUrl::parse("ws://localhost:8080/").unwrap();
        let localhost_without_slash = RelayUrl::parse("ws://localhost:8080").unwrap();

        assert_eq!(
            normalize_relay_url(&localhost_with_slash),
            "ws://localhost:8080"
        );
        assert_eq!(
            normalize_relay_url(&localhost_without_slash),
            "ws://localhost:8080"
        );

        // Test paths (should remove trailing slashes)
        let path_with_slash = RelayUrl::parse("wss://relay.com/nostr/").unwrap();
        let path_without_slash = RelayUrl::parse("wss://relay.com/nostr").unwrap();

        assert_eq!(
            normalize_relay_url(&path_with_slash),
            "wss://relay.com/nostr"
        );
        assert_eq!(
            normalize_relay_url(&path_without_slash),
            "wss://relay.com/nostr"
        );

        // Test deep paths (should remove trailing slashes)
        let deep_path_with_slash = RelayUrl::parse("wss://relay.com/path/sub/").unwrap();
        let deep_path_without_slash = RelayUrl::parse("wss://relay.com/path/sub").unwrap();

        assert_eq!(
            normalize_relay_url(&deep_path_with_slash),
            "wss://relay.com/path/sub"
        );
        assert_eq!(
            normalize_relay_url(&deep_path_without_slash),
            "wss://relay.com/path/sub"
        );

        // Test URLs with ports and paths (should remove trailing slashes)
        let port_path_with_slash = RelayUrl::parse("wss://relay.com:8443/nostr/").unwrap();
        let port_path_without_slash = RelayUrl::parse("wss://relay.com:8443/nostr").unwrap();

        assert_eq!(
            normalize_relay_url(&port_path_with_slash),
            "wss://relay.com:8443/nostr"
        );
        assert_eq!(
            normalize_relay_url(&port_path_without_slash),
            "wss://relay.com:8443/nostr"
        );

        // Test URLs with ports but no path (should remove trailing slash)
        let port_root_with_slash = RelayUrl::parse("wss://relay.com:8443/").unwrap();
        let port_root_without_slash = RelayUrl::parse("wss://relay.com:8443").unwrap();

        assert_eq!(
            normalize_relay_url(&port_root_with_slash),
            "wss://relay.com:8443"
        );
        assert_eq!(
            normalize_relay_url(&port_root_without_slash),
            "wss://relay.com:8443"
        );
    }

    #[tokio::test]
    async fn test_relay_find_by_url_normalization() {
        use crate::whitenoise::database::Database;
        use crate::whitenoise::relays::Relay;
        use std::path::PathBuf;

        let pool = setup_test_db().await;
        let database = Database {
            pool,
            path: PathBuf::from(":memory:"),
            last_connected: std::time::SystemTime::now(),
        };

        // Save a relay with trailing slash
        let url_with_slash = RelayUrl::parse("wss://test.relay.com/").unwrap();
        let saved_relay = Relay::new(&url_with_slash).save(&database).await.unwrap();

        // Try to find it with and without trailing slash - both should work
        let url_without_slash = RelayUrl::parse("wss://test.relay.com").unwrap();

        let found_with_slash = Relay::find_by_url(&url_with_slash, &database)
            .await
            .unwrap();
        let found_without_slash = Relay::find_by_url(&url_without_slash, &database)
            .await
            .unwrap();

        assert_eq!(found_with_slash.id, saved_relay.id);
        assert_eq!(found_without_slash.id, saved_relay.id);
    }

    #[tokio::test]
    async fn test_relay_save_normalization_prevents_duplicates() {
        use crate::whitenoise::database::Database;
        use crate::whitenoise::relays::Relay;
        use std::path::PathBuf;

        let pool = setup_test_db().await;
        let database = Database {
            pool,
            path: PathBuf::from(":memory:"),
            last_connected: std::time::SystemTime::now(),
        };

        // Save relay without trailing slash
        let url_without_slash = RelayUrl::parse("wss://test.relay.com").unwrap();
        let relay1 = Relay::new(&url_without_slash)
            .save(&database)
            .await
            .unwrap();

        // Save relay with trailing slash - should update existing, not create new
        let url_with_slash = RelayUrl::parse("wss://test.relay.com/").unwrap();
        let relay2 = Relay::new(&url_with_slash).save(&database).await.unwrap();

        // Should have same ID (no duplicate created)
        assert_eq!(relay1.id, relay2.id);

        // Updated_at should be newer for relay2
        assert!(relay2.updated_at >= relay1.updated_at);
    }

    #[tokio::test]
    async fn test_relay_normalization_with_paths() {
        use crate::whitenoise::database::Database;
        use crate::whitenoise::relays::Relay;
        use std::path::PathBuf;

        let pool = setup_test_db().await;
        let database = Database {
            pool,
            path: PathBuf::from(":memory:"),
            last_connected: std::time::SystemTime::now(),
        };

        // URLs with paths should be treated as duplicates when they differ only by trailing slash
        let path_without_slash = RelayUrl::parse("wss://test.relay.com/nostr").unwrap();
        let path_with_slash = RelayUrl::parse("wss://test.relay.com/nostr/").unwrap();

        let relay1 = Relay::new(&path_without_slash)
            .save(&database)
            .await
            .unwrap();
        let relay2 = Relay::new(&path_with_slash).save(&database).await.unwrap();

        // Should have same ID (normalized to same URL)
        assert_eq!(relay1.id, relay2.id);

        // Should be able to find the relay using either URL variant
        let found1 = Relay::find_by_url(&path_without_slash, &database)
            .await
            .unwrap();
        let found2 = Relay::find_by_url(&path_with_slash, &database)
            .await
            .unwrap();

        assert_eq!(found1.id, relay1.id);
        assert_eq!(found2.id, relay1.id);
        assert_eq!(found1.id, found2.id);
    }

    #[tokio::test]
    async fn test_relay_new_row_from_row_invalid_timestamps() {
        let pool = setup_test_db().await;

        let test_url = "wss://relay.damus.io";
        let valid_timestamp = chrono::Utc::now().timestamp_millis();
        let invalid_timestamp = i64::MAX; // This will be too large for DateTime conversion

        // Test invalid created_at timestamp
        sqlx::query("INSERT INTO relays (url, created_at, updated_at) VALUES (?, ?, ?)")
            .bind(test_url)
            .bind(invalid_timestamp)
            .bind(valid_timestamp)
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM relays WHERE url = ?")
            .bind(test_url)
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = RelayRow::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "created_at");
        } else {
            panic!("Expected ColumnDecode error for created_at timestamp");
        }

        // Clean up and test invalid updated_at timestamp
        sqlx::query("DELETE FROM relays WHERE url = ?")
            .bind(test_url)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO relays (url, created_at, updated_at) VALUES (?, ?, ?)")
            .bind(test_url)
            .bind(valid_timestamp)
            .bind(invalid_timestamp)
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM relays WHERE url = ?")
            .bind(test_url)
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = RelayRow::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "updated_at");
        } else {
            panic!("Expected ColumnDecode error for updated_at timestamp");
        }
    }
}
