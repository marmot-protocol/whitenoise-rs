#[cfg(test)]
use chrono::{DateTime, Utc};

#[cfg(test)]
use super::utils::parse_timestamp;
#[cfg(test)]
use crate::whitenoise::relays::RelayType;

#[cfg(test)]
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub(crate) struct UserRelay {
    // user_id is the ID of the user
    pub user_id: i64,
    // relay_id is the ID of the relay
    pub relay_id: i64,
    // relay_type is the type of the relay
    pub relay_type: RelayType,
    // created_at is the timestamp of the user relay creation
    pub created_at: DateTime<Utc>,
    // updated_at is the timestamp of the last update
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
impl<'r, R> sqlx::FromRow<'r, R> for UserRelay
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let user_id: i64 = row.try_get("user_id")?;
        let relay_id: i64 = row.try_get("relay_id")?;
        let relay_type_str: String = row.try_get("relay_type")?;

        let relay_type = relay_type_str
            .parse()
            .map_err(|e| sqlx::Error::ColumnDecode {
                index: "relay_type".to_string(),
                source: Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            })?;
        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(UserRelay {
            user_id,
            relay_id,
            relay_type,
            created_at,
            updated_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqlitePoolOptions, SqliteRow};
    use sqlx::{FromRow, SqlitePool};

    async fn setup_test_db() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        // Create the user_relays table
        sqlx::query(
            "CREATE TABLE user_relays (
                user_id INTEGER NOT NULL,
                relay_id INTEGER NOT NULL,
                relay_type TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (user_id, relay_id, relay_type)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    #[tokio::test]
    async fn test_user_relay_row_from_row_valid_data() {
        let pool = setup_test_db().await;

        let test_user_id = 1i64;
        let test_relay_id = 42i64;
        let test_relay_type = "nip65";
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        // Insert test data
        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(test_user_id)
        .bind(test_relay_id)
        .bind(test_relay_type)
        .bind(test_timestamp)
        .bind(test_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        // Test from_row implementation
        let row: SqliteRow =
            sqlx::query("SELECT * FROM user_relays WHERE user_id = ? AND relay_id = ?")
                .bind(test_user_id)
                .bind(test_relay_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        let user_relay_row = UserRelay::from_row(&row).unwrap();

        assert_eq!(user_relay_row.user_id, test_user_id);
        assert_eq!(user_relay_row.relay_id, test_relay_id);
        assert_eq!(user_relay_row.relay_type, RelayType::Nip65);
        assert_eq!(user_relay_row.created_at.timestamp_millis(), test_timestamp);
        assert_eq!(user_relay_row.updated_at.timestamp_millis(), test_timestamp);
    }

    #[tokio::test]
    async fn test_user_relay_row_from_row_all_relay_types() {
        let pool = setup_test_db().await;
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        let test_cases = [
            ("nip65", RelayType::Nip65),
            ("inbox", RelayType::Inbox),
            ("key_package", RelayType::KeyPackage),
            ("NIP65", RelayType::Nip65), // Test case insensitive
            ("INBOX", RelayType::Inbox),
            ("KEY_PACKAGE", RelayType::KeyPackage),
        ];

        for (i, (relay_type_str, expected_relay_type)) in test_cases.iter().enumerate() {
            let test_user_id = (i + 1) as i64;
            let test_relay_id = (i + 100) as i64;

            // Insert test data
            sqlx::query(
                "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(test_user_id)
            .bind(test_relay_id)
            .bind(relay_type_str)
            .bind(test_timestamp)
            .bind(test_timestamp)
            .execute(&pool)
            .await
            .unwrap();

            // Test from_row implementation
            let row: SqliteRow =
                sqlx::query("SELECT * FROM user_relays WHERE user_id = ? AND relay_id = ?")
                    .bind(test_user_id)
                    .bind(test_relay_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();

            let user_relay_row = UserRelay::from_row(&row).unwrap();

            assert_eq!(user_relay_row.user_id, test_user_id);
            assert_eq!(user_relay_row.relay_id, test_relay_id);
            assert_eq!(user_relay_row.relay_type, *expected_relay_type);
            assert_eq!(user_relay_row.created_at.timestamp_millis(), test_timestamp);
            assert_eq!(user_relay_row.updated_at.timestamp_millis(), test_timestamp);
        }
    }

    #[tokio::test]
    async fn test_user_relay_row_from_row_invalid_timestamps() {
        let pool = setup_test_db().await;

        let test_user_id = 1i64;
        let test_relay_id = 42i64;
        let test_relay_type = "nip65";
        let valid_timestamp = chrono::Utc::now().timestamp_millis();
        let invalid_timestamp = i64::MIN; // Invalid timestamp value

        // Test invalid created_at timestamp
        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(test_user_id)
        .bind(test_relay_id)
        .bind(test_relay_type)
        .bind(invalid_timestamp)
        .bind(valid_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow =
            sqlx::query("SELECT * FROM user_relays WHERE user_id = ? AND relay_id = ?")
                .bind(test_user_id)
                .bind(test_relay_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        let result = UserRelay::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "created_at");
        } else {
            panic!("Expected ColumnDecode error for created_at timestamp");
        }

        // Clean up and test invalid updated_at timestamp
        sqlx::query("DELETE FROM user_relays WHERE user_id = ? AND relay_id = ?")
            .bind(test_user_id)
            .bind(test_relay_id)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(test_user_id)
        .bind(test_relay_id)
        .bind(test_relay_type)
        .bind(valid_timestamp)
        .bind(invalid_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow =
            sqlx::query("SELECT * FROM user_relays WHERE user_id = ? AND relay_id = ?")
                .bind(test_user_id)
                .bind(test_relay_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        let result = UserRelay::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "updated_at");
        } else {
            panic!("Expected ColumnDecode error for updated_at timestamp");
        }
    }

    #[tokio::test]
    async fn test_user_relay_row_from_row_timestamp_edge_cases() {
        let pool = setup_test_db().await;

        let test_user_id = 1i64;
        let test_relay_id = 42i64;
        let test_relay_type = "inbox";

        // Test with timestamp 0 (Unix epoch)
        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(test_user_id)
        .bind(test_relay_id)
        .bind(test_relay_type)
        .bind(0i64)
        .bind(0i64)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM user_relays WHERE created_at = 0")
            .fetch_one(&pool)
            .await
            .unwrap();

        let user_relay_row = UserRelay::from_row(&row).unwrap();
        assert_eq!(user_relay_row.created_at.timestamp_millis(), 0);
        assert_eq!(user_relay_row.updated_at.timestamp_millis(), 0);
        assert_eq!(user_relay_row.relay_type, RelayType::Inbox);

        // Clean up
        sqlx::query("DELETE FROM user_relays WHERE created_at = 0")
            .execute(&pool)
            .await
            .unwrap();

        // Test with future timestamp
        let future_timestamp =
            (chrono::Utc::now() + chrono::Duration::days(365)).timestamp_millis();
        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(test_user_id)
        .bind(test_relay_id)
        .bind(test_relay_type)
        .bind(future_timestamp)
        .bind(future_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM user_relays WHERE created_at = ?")
            .bind(future_timestamp)
            .fetch_one(&pool)
            .await
            .unwrap();

        let user_relay_row = UserRelay::from_row(&row).unwrap();
        assert_eq!(
            user_relay_row.created_at.timestamp_millis(),
            future_timestamp
        );
        assert_eq!(
            user_relay_row.updated_at.timestamp_millis(),
            future_timestamp
        );
    }

    #[tokio::test]
    async fn test_user_relay_row_from_row_large_ids() {
        let pool = setup_test_db().await;

        let test_user_id = i64::MAX;
        let test_relay_id = i64::MAX - 1;
        let test_relay_type = "key_package";
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        // Test with very large IDs
        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(test_user_id)
        .bind(test_relay_id)
        .bind(test_relay_type)
        .bind(test_timestamp)
        .bind(test_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow =
            sqlx::query("SELECT * FROM user_relays WHERE user_id = ? AND relay_id = ?")
                .bind(test_user_id)
                .bind(test_relay_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        let user_relay_row = UserRelay::from_row(&row).unwrap();

        assert_eq!(user_relay_row.user_id, test_user_id);
        assert_eq!(user_relay_row.relay_id, test_relay_id);
        assert_eq!(user_relay_row.relay_type, RelayType::KeyPackage);
        assert_eq!(user_relay_row.created_at.timestamp_millis(), test_timestamp);
        assert_eq!(user_relay_row.updated_at.timestamp_millis(), test_timestamp);
    }

    #[tokio::test]
    async fn test_user_relay_row_from_row_negative_ids() {
        let pool = setup_test_db().await;

        let test_user_id = -1i64;
        let test_relay_id = -42i64;
        let test_relay_type = "nip65";
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        // Test with negative IDs (should work as they're just i64 values)
        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(test_user_id)
        .bind(test_relay_id)
        .bind(test_relay_type)
        .bind(test_timestamp)
        .bind(test_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow =
            sqlx::query("SELECT * FROM user_relays WHERE user_id = ? AND relay_id = ?")
                .bind(test_user_id)
                .bind(test_relay_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        let user_relay_row = UserRelay::from_row(&row).unwrap();

        assert_eq!(user_relay_row.user_id, test_user_id);
        assert_eq!(user_relay_row.relay_id, test_relay_id);
        assert_eq!(user_relay_row.relay_type, RelayType::Nip65);
        assert_eq!(user_relay_row.created_at.timestamp_millis(), test_timestamp);
        assert_eq!(user_relay_row.updated_at.timestamp_millis(), test_timestamp);
    }

    #[test]
    fn test_user_relay_row_debug_and_clone() {
        let timestamp = chrono::Utc::now();
        let user_relay_row = UserRelay {
            user_id: 1,
            relay_id: 42,
            relay_type: RelayType::Inbox,
            created_at: timestamp,
            updated_at: timestamp,
        };

        // Test debug formatting doesn't panic
        let debug_str = format!("{:?}", user_relay_row);
        assert!(debug_str.contains("UserRelay"));
        assert!(debug_str.contains("Inbox"));

        // Test clone
        let cloned_row = user_relay_row.clone();
        assert_eq!(user_relay_row, cloned_row);

        // Test hash (by using in a HashSet)
        let mut set = std::collections::HashSet::new();
        set.insert(user_relay_row);
        set.insert(cloned_row); // Should not increase size due to equality
        assert_eq!(set.len(), 1);
    }

    #[tokio::test]
    async fn test_user_relay_row_from_row_invalid_relay_type() {
        let pool = setup_test_db().await;

        let test_user_id = 1i64;
        let test_relay_id = 42i64;
        let invalid_relay_type = "invalid_type";
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT INTO user_relays (user_id, relay_id, relay_type, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(test_user_id)
        .bind(test_relay_id)
        .bind(invalid_relay_type)
        .bind(test_timestamp)
        .bind(test_timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow =
            sqlx::query("SELECT * FROM user_relays WHERE user_id = ? AND relay_id = ?")
                .bind(test_user_id)
                .bind(test_relay_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        let result = UserRelay::from_row(&row);
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "relay_type");
        } else {
            panic!("Expected ColumnDecode error for invalid relay_type");
        }
    }

    #[test]
    fn test_relay_type_conversion_roundtrip() {
        let test_cases = vec![RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage];

        for relay_type in test_cases {
            let type_str: String = relay_type.into();
            let parsed_type = type_str.parse().unwrap();
            assert_eq!(relay_type, parsed_type);
        }
    }
}
