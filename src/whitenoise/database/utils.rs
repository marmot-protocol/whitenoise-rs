use chrono::{DateTime, NaiveDateTime, Utc};
use nostr_sdk::{PublicKey, RelayUrl};
use sqlx::Row;

use crate::relay_control::{
    RelayPlane,
    observability::{RelayFailureCategory, RelayTelemetryKind},
};

/// Parses a timestamp column with flexible type handling for SQLite type affinity.
///
/// This function gracefully handles SQLite's type affinity by trying to parse
/// the column as both INTEGER (milliseconds since Unix epoch) and TEXT
/// (datetime string). This allows the application to work with mixed data
/// that may have been inserted using different methods.
///
/// # Arguments
/// * `row` - The database row to parse from
/// * `column_name` - Name of the timestamp column to parse
///
/// # Returns
/// * `Ok(DateTime<Utc>)` - Successfully parsed timestamp
/// * `Err(sqlx::Error)` - Column doesn't exist or couldn't be parsed as either format
///
/// # Examples
/// ```ignore
/// // Works with INTEGER timestamps (milliseconds)
/// let timestamp = parse_timestamp(&row, "created_at")?;
///
/// // Also works with TEXT timestamps ("2025-08-16 11:34:29")
/// let timestamp = parse_timestamp(&row, "updated_at")?;
/// ```
pub(crate) fn parse_timestamp<'r, R>(
    row: &'r R,
    column_name: &'r str,
) -> Result<DateTime<Utc>, sqlx::Error>
where
    R: Row,
    &'r str: sqlx::ColumnIndex<R>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    // Try INTEGER timestamp first (milliseconds)
    if let Ok(timestamp_ms) = row.try_get::<i64, _>(column_name) {
        return DateTime::from_timestamp_millis(timestamp_ms)
            .ok_or_else(|| create_column_decode_error(column_name, "Invalid timestamp value"));
    }

    // Fall back to TEXT datetime string
    if let Ok(datetime_str) = row.try_get::<String, _>(column_name) {
        return parse_datetime_string(&datetime_str, column_name);
    }

    Err(create_column_decode_error(
        column_name,
        "Could not parse as INTEGER or DATETIME",
    ))
}

/// Parses an optional timestamp column with the same INTEGER/TEXT flexibility as
/// [`parse_timestamp`].
pub(crate) fn parse_optional_timestamp<'r, R>(
    row: &'r R,
    column_name: &'r str,
) -> Result<Option<DateTime<Utc>>, sqlx::Error>
where
    R: Row,
    &'r str: sqlx::ColumnIndex<R>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    if let Ok(timestamp_ms) = row.try_get::<Option<i64>, _>(column_name) {
        return timestamp_ms
            .map(|value| {
                DateTime::from_timestamp_millis(value).ok_or_else(|| {
                    create_column_decode_error(column_name, "Invalid timestamp value")
                })
            })
            .transpose();
    }

    if let Ok(datetime_str) = row.try_get::<Option<String>, _>(column_name) {
        return datetime_str
            .map(|value| parse_datetime_string(&value, column_name))
            .transpose();
    }

    Err(create_column_decode_error(
        column_name,
        "Could not parse optional timestamp as INTEGER or DATETIME",
    ))
}

/// Normalizes a RelayUrl to ensure consistent database storage.
pub(crate) fn normalize_relay_url(url: &RelayUrl) -> String {
    url.to_string().trim_end_matches('/').to_string()
}

pub(crate) fn parse_relay_url(value: String) -> Result<RelayUrl, sqlx::Error> {
    RelayUrl::parse(&value).map_err(|error| sqlx::Error::ColumnDecode {
        index: "relay_url".to_string(),
        source: Box::new(error),
    })
}

pub(crate) fn parse_relay_plane(value: String) -> Result<RelayPlane, sqlx::Error> {
    value
        .parse::<RelayPlane>()
        .map_err(|error| create_column_decode_error("plane", &error))
}

pub(crate) fn parse_telemetry_kind(value: String) -> Result<RelayTelemetryKind, sqlx::Error> {
    value
        .parse::<RelayTelemetryKind>()
        .map_err(|error| create_column_decode_error("telemetry_kind", &error))
}

pub(crate) fn parse_failure_category(value: String) -> Result<RelayFailureCategory, sqlx::Error> {
    value
        .parse::<RelayFailureCategory>()
        .map_err(|error| create_column_decode_error("failure_category", &error))
}

pub(crate) fn parse_optional_public_key(
    value: Option<String>,
) -> Result<Option<PublicKey>, sqlx::Error> {
    let Some(value) = value else {
        return Ok(None);
    };

    PublicKey::from_hex(&value)
        .map(Some)
        .map_err(|error| create_column_decode_error("account_pubkey", &error.to_string()))
}

pub(crate) fn serialize_optional_public_key(account_pubkey: Option<PublicKey>) -> Option<String> {
    account_pubkey.map(|pubkey| pubkey.to_hex())
}

fn parse_datetime_string(
    datetime_str: &str,
    column_name: &str,
) -> Result<DateTime<Utc>, sqlx::Error> {
    // First, try RFC3339 if it looks like one (contains 'T' or timezone indicators)
    if datetime_str.contains('T')
        || datetime_str.contains('+')
        || datetime_str.contains('Z')
        || datetime_str.ends_with("UTC")
    {
        if let Ok(dt) = DateTime::parse_from_rfc3339(datetime_str) {
            return Ok(dt.with_timezone(&Utc));
        }
        // Also try direct UTC parsing for strings that might already be in UTC format
        if let Ok(dt) = datetime_str.parse::<DateTime<Utc>>() {
            return Ok(dt);
        }
    }

    // Fall back to parsing common SQLite TEXT formats with NaiveDateTime
    let formats = [
        "%Y-%m-%d %H:%M:%S%.f", // With optional fractional seconds
        "%Y-%m-%d %H:%M:%S",    // Without fractional seconds
        "%Y-%m-%d",             // Date only (assumes midnight)
    ];

    for format in &formats {
        if let Ok(naive_dt) = NaiveDateTime::parse_from_str(datetime_str, format) {
            return Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive_dt, Utc));
        }
    }

    // If it's just a date, try parsing and assume midnight
    if let Ok(naive_date) = chrono::NaiveDate::parse_from_str(datetime_str, "%Y-%m-%d") {
        let naive_dt = naive_date.and_hms_opt(0, 0, 0).unwrap();
        return Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive_dt, Utc));
    }

    // All parsing attempts failed
    Err(sqlx::Error::ColumnDecode {
        index: column_name.to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Could not parse datetime string: '{}'", datetime_str),
        )),
    })
}

/// Helper function to create consistent ColumnDecode errors.
pub(crate) fn create_column_decode_error(column_name: &str, message: &str) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: column_name.to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};
    use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};

    async fn setup_test_db() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE test_timestamps (
                id INTEGER PRIMARY KEY,
                int_timestamp INTEGER,
                text_timestamp TEXT,
                datetime_timestamp DATETIME,
                invalid_int INTEGER,
                invalid_text TEXT
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    #[tokio::test]
    async fn test_parse_timestamp_integer_valid() {
        let pool = setup_test_db().await;
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        sqlx::query("INSERT INTO test_timestamps (id, int_timestamp) VALUES (1, ?)")
            .bind(test_timestamp)
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = parse_timestamp(&row, "int_timestamp");
        assert!(result.is_ok());

        let parsed_time = result.unwrap();
        assert_eq!(parsed_time.timestamp_millis(), test_timestamp);
    }

    #[tokio::test]
    async fn test_parse_timestamp_text_valid() {
        let pool = setup_test_db().await;

        sqlx::query("INSERT INTO test_timestamps (id, text_timestamp) VALUES (1, ?)")
            .bind("2025-08-16 11:34:29")
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = parse_timestamp(&row, "text_timestamp");
        assert!(result.is_ok());

        let parsed_time = result.unwrap();
        assert_eq!(parsed_time.year(), 2025);
        assert_eq!(parsed_time.month(), 8);
        assert_eq!(parsed_time.day(), 16);
        assert_eq!(parsed_time.hour(), 11);
        assert_eq!(parsed_time.minute(), 34);
        assert_eq!(parsed_time.second(), 29);
    }

    #[tokio::test]
    async fn test_parse_timestamp_text_with_subseconds() {
        let pool = setup_test_db().await;

        sqlx::query("INSERT INTO test_timestamps (id, text_timestamp) VALUES (1, ?)")
            .bind("2025-08-16 11:34:29.123")
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = parse_timestamp(&row, "text_timestamp");
        assert!(result.is_ok());

        let parsed_time = result.unwrap();
        assert_eq!(parsed_time.timestamp_subsec_millis(), 123);
    }

    #[tokio::test]
    async fn test_parse_timestamp_invalid_integer() {
        let pool = setup_test_db().await;

        // Use i64::MIN which should be invalid for DateTime::from_timestamp_millis
        sqlx::query("INSERT INTO test_timestamps (id, invalid_int) VALUES (1, ?)")
            .bind(i64::MIN)
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = parse_timestamp(&row, "invalid_int");
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "invalid_int");
        } else {
            panic!("Expected ColumnDecode error");
        }
    }

    #[tokio::test]
    async fn test_parse_timestamp_invalid_text() {
        let pool = setup_test_db().await;

        sqlx::query("INSERT INTO test_timestamps (id, invalid_text) VALUES (1, ?)")
            .bind("not a timestamp")
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = parse_timestamp(&row, "invalid_text");
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "invalid_text");
        } else {
            panic!("Expected ColumnDecode error");
        }
    }

    #[tokio::test]
    async fn test_parse_timestamp_nonexistent_column() {
        let pool = setup_test_db().await;

        sqlx::query("INSERT INTO test_timestamps (id) VALUES (1)")
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = parse_timestamp(&row, "nonexistent_column");
        assert!(result.is_err());

        if let Err(sqlx::Error::ColumnDecode { index, .. }) = result {
            assert_eq!(index, "nonexistent_column");
        } else {
            panic!("Expected ColumnDecode error");
        }
    }

    #[tokio::test]
    async fn test_parse_timestamp_text_without_subseconds() {
        let pool = setup_test_db().await;

        sqlx::query("INSERT INTO test_timestamps (id, text_timestamp) VALUES (1, ?)")
            .bind("2025-08-16 11:34:29") // No subseconds
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = parse_timestamp(&row, "text_timestamp");
        assert!(result.is_ok());

        let parsed_time = result.unwrap();
        assert_eq!(parsed_time.timestamp_subsec_millis(), 0); // Should be 0 when no subseconds
        assert_eq!(parsed_time.year(), 2025);
        assert_eq!(parsed_time.month(), 8);
        assert_eq!(parsed_time.day(), 16);
        assert_eq!(parsed_time.hour(), 11);
        assert_eq!(parsed_time.minute(), 34);
        assert_eq!(parsed_time.second(), 29);
    }

    #[tokio::test]
    async fn test_parse_timestamp_integer_subsecond_precision() {
        let pool = setup_test_db().await;

        // Test specific millisecond values
        let test_cases = [
            1755343067000, // Exact seconds (no subseconds)
            1755343067123, // 123 milliseconds
            1755343067456, // 456 milliseconds
            1755343067999, // 999 milliseconds (max)
        ];

        for (i, test_timestamp) in test_cases.iter().enumerate() {
            let id = i + 1;
            sqlx::query("INSERT INTO test_timestamps (id, int_timestamp) VALUES (?, ?)")
                .bind(id as i64)
                .bind(*test_timestamp)
                .execute(&pool)
                .await
                .unwrap();

            let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = ?")
                .bind(id as i64)
                .fetch_one(&pool)
                .await
                .unwrap();

            let result = parse_timestamp(&row, "int_timestamp");
            assert!(result.is_ok());

            let parsed_time = result.unwrap();
            assert_eq!(parsed_time.timestamp_millis(), *test_timestamp);

            // Verify subsecond precision is preserved
            let expected_subsec = (*test_timestamp % 1000) as u32;
            assert_eq!(parsed_time.timestamp_subsec_millis(), expected_subsec);
        }
    }

    #[tokio::test]
    async fn test_parse_timestamp_text_various_subsecond_formats() {
        let pool = setup_test_db().await;

        let test_cases = [
            ("2025-08-16 11:34:29", 0),       // No subseconds
            ("2025-08-16 11:34:29.1", 100),   // Single digit subseconds
            ("2025-08-16 11:34:29.12", 120),  // Two digit subseconds
            ("2025-08-16 11:34:29.123", 123), // Three digit subseconds
            ("2025-08-16 11:34:29.000", 0),   // Explicit zero subseconds
        ];

        for (i, (timestamp_str, expected_millis)) in test_cases.iter().enumerate() {
            let id = i + 1;

            // Clear previous data
            sqlx::query("DELETE FROM test_timestamps")
                .execute(&pool)
                .await
                .unwrap();

            sqlx::query("INSERT INTO test_timestamps (id, text_timestamp) VALUES (?, ?)")
                .bind(id as i64)
                .bind(timestamp_str)
                .execute(&pool)
                .await
                .unwrap();

            let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = ?")
                .bind(id as i64)
                .fetch_one(&pool)
                .await
                .unwrap();

            let result = parse_timestamp(&row, "text_timestamp");
            assert!(result.is_ok(), "Failed to parse: {timestamp_str}");

            let parsed_time = result.unwrap();
            assert_eq!(
                parsed_time.timestamp_subsec_millis(),
                *expected_millis,
                "Subsecond mismatch for: {} (expected: {}, got: {})",
                timestamp_str,
                expected_millis,
                parsed_time.timestamp_subsec_millis()
            );
        }
    }

    #[tokio::test]
    async fn test_parse_timestamp_rfc3339_formats() {
        let pool = setup_test_db().await;

        let test_cases = [
            "2025-08-16T11:34:29Z",
            "2025-08-16T11:34:29+00:00",
            "2025-08-16T11:34:29.123Z",
            "2025-08-16T11:34:29.123+00:00",
            "2025-08-16T11:34:29-05:00",
        ];

        for (i, timestamp_str) in test_cases.iter().enumerate() {
            let id = i + 1;

            // Clear previous data
            sqlx::query("DELETE FROM test_timestamps")
                .execute(&pool)
                .await
                .unwrap();

            sqlx::query("INSERT INTO test_timestamps (id, text_timestamp) VALUES (?, ?)")
                .bind(id as i64)
                .bind(timestamp_str)
                .execute(&pool)
                .await
                .unwrap();

            let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = ?")
                .bind(id as i64)
                .fetch_one(&pool)
                .await
                .unwrap();

            let result = parse_timestamp(&row, "text_timestamp");
            assert!(result.is_ok(), "Failed to parse RFC3339: {timestamp_str}");

            let parsed_time = result.unwrap();
            assert_eq!(parsed_time.year(), 2025);
            assert_eq!(parsed_time.month(), 8);
            assert_eq!(parsed_time.day(), 16);
        }
    }

    #[tokio::test]
    async fn test_parse_timestamp_date_only_format() {
        let pool = setup_test_db().await;

        sqlx::query("INSERT INTO test_timestamps (id, text_timestamp) VALUES (1, ?)")
            .bind("2025-08-16")
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = parse_timestamp(&row, "text_timestamp");
        assert!(result.is_ok());

        let parsed_time = result.unwrap();
        assert_eq!(parsed_time.year(), 2025);
        assert_eq!(parsed_time.month(), 8);
        assert_eq!(parsed_time.day(), 16);
        assert_eq!(parsed_time.hour(), 0);
        assert_eq!(parsed_time.minute(), 0);
        assert_eq!(parsed_time.second(), 0);
    }

    #[tokio::test]
    async fn test_parse_datetime_string_edge_cases() {
        // Test the parse_datetime_string function directly

        // RFC3339 formats
        assert!(parse_datetime_string("2025-08-16T11:34:29Z", "test").is_ok());
        assert!(parse_datetime_string("2025-08-16T11:34:29+00:00", "test").is_ok());
        assert!(parse_datetime_string("2025-08-16T11:34:29.123Z", "test").is_ok());

        // SQLite TEXT formats
        assert!(parse_datetime_string("2025-08-16 11:34:29", "test").is_ok());
        assert!(parse_datetime_string("2025-08-16 11:34:29.123", "test").is_ok());
        assert!(parse_datetime_string("2025-08-16", "test").is_ok());

        // Invalid formats
        assert!(parse_datetime_string("not a date", "test").is_err());
        assert!(parse_datetime_string("2025-13-50", "test").is_err());
        assert!(parse_datetime_string("", "test").is_err());

        // Verify error message contains the invalid string
        if let Err(sqlx::Error::ColumnDecode { source, .. }) =
            parse_datetime_string("invalid", "test")
        {
            let error_msg = format!("{source}");
            assert!(error_msg.contains("invalid"));
        } else {
            panic!("Expected ColumnDecode error with source");
        }
    }

    #[tokio::test]
    async fn test_parse_timestamp_datetime_column() {
        let pool = setup_test_db().await;

        sqlx::query("INSERT INTO test_timestamps (id, datetime_timestamp) VALUES (1, ?)")
            .bind("2025-08-16 11:34:29.456")
            .execute(&pool)
            .await
            .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = parse_timestamp(&row, "datetime_timestamp");
        assert!(result.is_ok());

        let parsed_time = result.unwrap();
        assert_eq!(parsed_time.year(), 2025);
        assert_eq!(parsed_time.month(), 8);
        assert_eq!(parsed_time.day(), 16);
        assert_eq!(parsed_time.hour(), 11);
        assert_eq!(parsed_time.minute(), 34);
        assert_eq!(parsed_time.second(), 29);
        assert_eq!(parsed_time.timestamp_subsec_millis(), 456);
    }

    #[tokio::test]
    async fn test_parse_timestamp_priority_integer_over_text() {
        let pool = setup_test_db().await;
        let test_timestamp = chrono::Utc::now().timestamp_millis();

        // Insert both integer and text values - integer should take priority
        sqlx::query(
            "INSERT INTO test_timestamps (id, int_timestamp, text_timestamp) VALUES (1, ?, ?)",
        )
        .bind(test_timestamp)
        .bind("2020-01-01 00:00:00") // Different date to verify integer is used
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM test_timestamps WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        // When both are available, should parse as integer (the current timestamp, not 2020)
        let result = parse_timestamp(&row, "int_timestamp");
        assert!(result.is_ok());

        let parsed_time = result.unwrap();
        assert_eq!(parsed_time.timestamp_millis(), test_timestamp);
        assert!(parsed_time.year() > 2020); // Should be recent timestamp, not 2020
    }

    #[test]
    fn test_parse_relay_url_valid() {
        let result = parse_relay_url("wss://relay.example.com".to_string());
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_relay_url_invalid() {
        let result = parse_relay_url("not a url".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_optional_public_key_none() {
        let result = parse_optional_public_key(None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_parse_optional_public_key_valid() {
        let keys = nostr_sdk::Keys::generate();
        let hex = keys.public_key().to_hex();
        let result = parse_optional_public_key(Some(hex));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().unwrap(), keys.public_key());
    }

    #[test]
    fn test_parse_optional_public_key_invalid() {
        let result = parse_optional_public_key(Some("not_a_valid_hex_key".to_string()));
        assert!(result.is_err());
    }

    #[test]
    fn test_serialize_optional_public_key_some() {
        let keys = nostr_sdk::Keys::generate();
        let result = serialize_optional_public_key(Some(keys.public_key()));
        assert_eq!(result, Some(keys.public_key().to_hex()));
    }

    #[test]
    fn test_serialize_optional_public_key_none() {
        let result = serialize_optional_public_key(None);
        assert!(result.is_none());
    }

    #[test]
    fn test_create_column_decode_error() {
        let err = create_column_decode_error("test_col", "bad value");
        match err {
            sqlx::Error::ColumnDecode { index, .. } => {
                assert_eq!(index, "test_col");
            }
            _ => panic!("Expected ColumnDecode error"),
        }
    }
}
