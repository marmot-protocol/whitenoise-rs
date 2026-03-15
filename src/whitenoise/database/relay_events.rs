use chrono::{DateTime, Utc};
use nostr_sdk::{PublicKey, RelayUrl};

use super::{
    Database, DatabaseError,
    utils::{
        normalize_relay_url, parse_failure_category, parse_optional_public_key, parse_relay_plane,
        parse_relay_url, parse_telemetry_kind, parse_timestamp, serialize_optional_public_key,
    },
};
use crate::perf_span;
use crate::relay_control::{
    RelayPlane,
    observability::{RelayFailureCategory, RelayTelemetry, RelayTelemetryKind},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayEventRecord {
    pub(crate) id: i64,
    pub(crate) relay_url: RelayUrl,
    pub(crate) plane: RelayPlane,
    pub(crate) account_pubkey: Option<PublicKey>,
    pub(crate) occurred_at: DateTime<Utc>,
    pub(crate) kind: RelayTelemetryKind,
    pub(crate) subscription_id: Option<String>,
    pub(crate) failure_category: Option<RelayFailureCategory>,
    pub(crate) message: Option<String>,
}

impl<'r, R> sqlx::FromRow<'r, R> for RelayEventRecord
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let relay_url = parse_relay_url(row.try_get::<String, _>("relay_url")?)?;
        let plane = parse_relay_plane(row.try_get::<String, _>("plane")?)?;
        let account_pubkey =
            parse_optional_public_key(row.try_get::<Option<String>, _>("account_pubkey")?)?;
        let occurred_at = parse_timestamp(row, "occurred_at")?;
        let kind = parse_telemetry_kind(row.try_get::<String, _>("telemetry_kind")?)?;
        let subscription_id: Option<String> = row.try_get("subscription_id")?;
        let failure_category = row
            .try_get::<Option<String>, _>("failure_category")?
            .map(parse_failure_category)
            .transpose()?;
        let message: Option<String> = row.try_get("message")?;

        Ok(Self {
            id,
            relay_url,
            plane,
            account_pubkey,
            occurred_at,
            kind,
            subscription_id,
            failure_category,
            message,
        })
    }
}

impl RelayEventRecord {
    pub(crate) async fn create(
        telemetry: &RelayTelemetry,
        database: &Database,
    ) -> Result<(), DatabaseError> {
        let _span = perf_span!("db::relay_event_create");
        sqlx::query(
            "INSERT INTO relay_events (
                relay_url,
                plane,
                account_pubkey,
                occurred_at,
                telemetry_kind,
                subscription_id,
                failure_category,
                message
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(normalize_relay_url(&telemetry.relay_url))
        .bind(telemetry.plane.as_str())
        .bind(serialize_optional_public_key(telemetry.account_pubkey))
        .bind(telemetry.occurred_at.timestamp_millis())
        .bind(telemetry.kind.as_str())
        .bind(telemetry.subscription_id.clone())
        .bind(
            telemetry
                .failure_category
                .map(|category| category.as_str().to_string()),
        )
        .bind(telemetry.message.clone())
        .execute(&database.pool)
        .await?;

        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) async fn list_recent_for_scope(
        relay_url: &RelayUrl,
        plane: RelayPlane,
        account_pubkey: Option<PublicKey>,
        limit: usize,
        database: &Database,
    ) -> Result<Vec<Self>, DatabaseError> {
        let _span = perf_span!("db::relay_event_list_recent_for_scope");
        let limit = i64::try_from(limit).map_err(|_| {
            DatabaseError::Sqlx(sqlx::Error::Protocol("limit overflow".to_string()))
        })?;

        let records = match account_pubkey {
            Some(account_pubkey) => {
                sqlx::query_as::<_, Self>(
                    "SELECT
                        id,
                        relay_url,
                        plane,
                        account_pubkey,
                        occurred_at,
                        telemetry_kind,
                        subscription_id,
                        failure_category,
                        message
                     FROM relay_events
                     WHERE relay_url = ? AND plane = ? AND account_pubkey = ?
                     ORDER BY occurred_at DESC, id DESC
                     LIMIT ?",
                )
                .bind(normalize_relay_url(relay_url))
                .bind(plane.as_str())
                .bind(account_pubkey.to_hex())
                .bind(limit)
                .fetch_all(&database.pool)
                .await?
            }
            None => {
                sqlx::query_as::<_, Self>(
                    "SELECT
                        id,
                        relay_url,
                        plane,
                        account_pubkey,
                        occurred_at,
                        telemetry_kind,
                        subscription_id,
                        failure_category,
                        message
                     FROM relay_events
                     WHERE relay_url = ? AND plane = ? AND account_pubkey IS NULL
                     ORDER BY occurred_at DESC, id DESC
                     LIMIT ?",
                )
                .bind(normalize_relay_url(relay_url))
                .bind(plane.as_str())
                .bind(limit)
                .fetch_all(&database.pool)
                .await?
            }
        };

        Ok(records)
    }
}
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use chrono::TimeZone;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    async fn setup_test_db() -> Database {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE relay_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                relay_url TEXT NOT NULL,
                plane TEXT NOT NULL,
                account_pubkey TEXT,
                occurred_at INTEGER NOT NULL,
                telemetry_kind TEXT NOT NULL,
                subscription_id TEXT,
                failure_category TEXT,
                message TEXT
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        Database {
            pool,
            path: PathBuf::from(":memory:"),
            last_connected: SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn test_create_and_list_recent_for_scope() {
        let database = setup_test_db().await;
        let relay_url = RelayUrl::parse("wss://relay.example.com/").unwrap();
        let account_pubkey =
            PublicKey::from_hex("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap();

        let older = RelayTelemetry::new(
            RelayTelemetryKind::Connected,
            RelayPlane::Discovery,
            relay_url.clone(),
        )
        .with_account_pubkey(account_pubkey)
        .with_occurred_at(Utc.with_ymd_and_hms(2026, 3, 7, 9, 0, 0).unwrap());
        let newer = RelayTelemetry::notice(RelayPlane::Discovery, relay_url.clone(), "rate limit")
            .with_account_pubkey(account_pubkey)
            .with_occurred_at(Utc.with_ymd_and_hms(2026, 3, 7, 10, 0, 0).unwrap())
            .with_subscription_id("sub-1");

        RelayEventRecord::create(&older, &database).await.unwrap();
        RelayEventRecord::create(&newer, &database).await.unwrap();

        let events = RelayEventRecord::list_recent_for_scope(
            &relay_url,
            RelayPlane::Discovery,
            Some(account_pubkey),
            10,
            &database,
        )
        .await
        .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, RelayTelemetryKind::Notice);
        assert_eq!(
            events[0].failure_category,
            Some(RelayFailureCategory::RateLimited)
        );
        assert_eq!(events[0].subscription_id.as_deref(), Some("sub-1"));
        assert_eq!(events[1].kind, RelayTelemetryKind::Connected);
        assert_eq!(
            events[0].relay_url,
            RelayUrl::parse("wss://relay.example.com").unwrap()
        );
    }

    #[tokio::test]
    async fn test_from_row_empty_account_scope_becomes_none() {
        let database = setup_test_db().await;
        let timestamp = Utc.with_ymd_and_hms(2026, 3, 7, 11, 0, 0).unwrap();

        sqlx::query(
            "INSERT INTO relay_events (
                relay_url,
                plane,
                account_pubkey,
                occurred_at,
                telemetry_kind,
                subscription_id,
                failure_category,
                message
            ) VALUES (?, ?, NULL, ?, ?, NULL, ?, ?)",
        )
        .bind("wss://relay.example.com")
        .bind("ephemeral")
        .bind(timestamp.timestamp_millis())
        .bind("query_failure")
        .bind("timeout")
        .bind("timed out")
        .execute(&database.pool)
        .await
        .unwrap();

        let record = sqlx::query_as::<_, RelayEventRecord>(
            "SELECT
                id,
                relay_url,
                plane,
                account_pubkey,
                occurred_at,
                telemetry_kind,
                subscription_id,
                failure_category,
                message
             FROM relay_events",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();

        assert_eq!(record.account_pubkey, None);
        assert_eq!(record.kind, RelayTelemetryKind::QueryFailure);
        assert_eq!(record.failure_category, Some(RelayFailureCategory::Timeout));
        assert_eq!(record.occurred_at, timestamp);
    }
}
