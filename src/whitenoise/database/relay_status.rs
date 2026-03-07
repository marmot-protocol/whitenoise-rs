use chrono::{DateTime, Utc};
use nostr_sdk::{PublicKey, RelayUrl};

use super::{
    Database, DatabaseError,
    utils::{
        create_column_decode_error, normalize_relay_url, parse_optional_timestamp, parse_timestamp,
    },
};
use crate::relay_control::{
    RelayPlane,
    observability::{RelayFailureCategory, RelayTelemetry, RelayTelemetryKind},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayStatusRecord {
    pub(crate) id: i64,
    pub(crate) relay_url: RelayUrl,
    pub(crate) plane: RelayPlane,
    pub(crate) account_pubkey: Option<PublicKey>,
    pub(crate) last_connect_attempt_at: Option<DateTime<Utc>>,
    pub(crate) last_connect_success_at: Option<DateTime<Utc>>,
    pub(crate) last_failure_at: Option<DateTime<Utc>>,
    pub(crate) failure_category: Option<RelayFailureCategory>,
    pub(crate) last_notice_reason: Option<String>,
    pub(crate) last_closed_reason: Option<String>,
    pub(crate) last_auth_reason: Option<String>,
    pub(crate) auth_required: bool,
    pub(crate) success_count: i64,
    pub(crate) failure_count: i64,
    pub(crate) latency_ms: Option<i64>,
    pub(crate) backoff_until: Option<DateTime<Utc>>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for RelayStatusRecord
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
            parse_optional_public_key(row.try_get::<String, _>("account_pubkey")?)?;
        let last_connect_attempt_at = parse_optional_timestamp(row, "last_connect_attempt_at")?;
        let last_connect_success_at = parse_optional_timestamp(row, "last_connect_success_at")?;
        let last_failure_at = parse_optional_timestamp(row, "last_failure_at")?;
        let failure_category = match row.try_get::<Option<String>, _>("failure_category")? {
            Some(value) => Some(parse_failure_category(value)?),
            None => None,
        };
        let last_notice_reason: Option<String> = row.try_get("last_notice_reason")?;
        let last_closed_reason: Option<String> = row.try_get("last_closed_reason")?;
        let last_auth_reason: Option<String> = row.try_get("last_auth_reason")?;
        let auth_required = row.try_get::<i64, _>("auth_required")? != 0;
        let success_count: i64 = row.try_get("success_count")?;
        let failure_count: i64 = row.try_get("failure_count")?;
        let latency_ms: Option<i64> = row.try_get("latency_ms")?;
        let backoff_until = parse_optional_timestamp(row, "backoff_until")?;
        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(Self {
            id,
            relay_url,
            plane,
            account_pubkey,
            last_connect_attempt_at,
            last_connect_success_at,
            last_failure_at,
            failure_category,
            last_notice_reason,
            last_closed_reason,
            last_auth_reason,
            auth_required,
            success_count,
            failure_count,
            latency_ms,
            backoff_until,
            created_at,
            updated_at,
        })
    }
}

impl RelayStatusRecord {
    pub(crate) async fn find(
        relay_url: &RelayUrl,
        plane: RelayPlane,
        account_pubkey: Option<PublicKey>,
        database: &Database,
    ) -> Result<Option<Self>, DatabaseError> {
        let record = sqlx::query_as::<_, Self>(
            "SELECT
                id,
                relay_url,
                plane,
                account_pubkey,
                last_connect_attempt_at,
                last_connect_success_at,
                last_failure_at,
                failure_category,
                last_notice_reason,
                last_closed_reason,
                last_auth_reason,
                auth_required,
                success_count,
                failure_count,
                latency_ms,
                backoff_until,
                created_at,
                updated_at
             FROM relay_status
             WHERE relay_url = ? AND plane = ? AND account_pubkey = ?",
        )
        .bind(normalize_relay_url(relay_url))
        .bind(plane.as_str())
        .bind(serialize_optional_public_key(account_pubkey))
        .fetch_optional(&database.pool)
        .await?;

        Ok(record)
    }

    pub(crate) async fn upsert_from_telemetry(
        telemetry: &RelayTelemetry,
        database: &Database,
    ) -> Result<(), DatabaseError> {
        let mut record = match Self::find(
            &telemetry.relay_url,
            telemetry.plane,
            telemetry.account_pubkey,
            database,
        )
        .await?
        {
            Some(existing) => existing,
            None => Self::new_scope(
                telemetry.relay_url.clone(),
                telemetry.plane,
                telemetry.account_pubkey,
                telemetry.occurred_at,
            ),
        };

        record.apply_telemetry(telemetry);
        record.save(database).await
    }

    fn new_scope(
        relay_url: RelayUrl,
        plane: RelayPlane,
        account_pubkey: Option<PublicKey>,
        timestamp: DateTime<Utc>,
    ) -> Self {
        Self {
            id: 0,
            relay_url,
            plane,
            account_pubkey,
            last_connect_attempt_at: None,
            last_connect_success_at: None,
            last_failure_at: None,
            failure_category: None,
            last_notice_reason: None,
            last_closed_reason: None,
            last_auth_reason: None,
            auth_required: false,
            success_count: 0,
            failure_count: 0,
            latency_ms: None,
            backoff_until: None,
            created_at: timestamp,
            updated_at: timestamp,
        }
    }

    fn apply_telemetry(&mut self, telemetry: &RelayTelemetry) {
        self.updated_at = telemetry.occurred_at;

        match telemetry.kind {
            RelayTelemetryKind::Connected => {
                self.last_connect_attempt_at = Some(telemetry.occurred_at);
                self.last_connect_success_at = Some(telemetry.occurred_at);
                self.backoff_until = None;
                self.success_count += 1;
            }
            RelayTelemetryKind::Disconnected => {
                self.last_connect_attempt_at = Some(telemetry.occurred_at);
                self.record_failure(telemetry);
            }
            RelayTelemetryKind::Notice => {
                self.last_notice_reason = telemetry.message.clone();
                self.record_failure(telemetry);
            }
            RelayTelemetryKind::Closed => {
                self.last_closed_reason = telemetry.message.clone();
                self.record_failure(telemetry);
            }
            RelayTelemetryKind::AuthChallenge => {
                self.last_auth_reason = telemetry.message.clone();
                self.auth_required = true;
                self.record_failure(telemetry);
            }
            RelayTelemetryKind::PublishAttempt
            | RelayTelemetryKind::QueryAttempt
            | RelayTelemetryKind::SubscriptionAttempt => {}
            RelayTelemetryKind::PublishSuccess
            | RelayTelemetryKind::QuerySuccess
            | RelayTelemetryKind::SubscriptionSuccess => {
                self.success_count += 1;
            }
            RelayTelemetryKind::PublishFailure
            | RelayTelemetryKind::QueryFailure
            | RelayTelemetryKind::SubscriptionFailure => {
                self.record_failure(telemetry);
            }
        }

        if matches!(
            telemetry.failure_category,
            Some(RelayFailureCategory::AuthRequired | RelayFailureCategory::AuthFailed)
        ) {
            self.auth_required = true;
        }
    }

    fn record_failure(&mut self, telemetry: &RelayTelemetry) {
        self.last_failure_at = Some(telemetry.occurred_at);
        self.failure_count += 1;

        if let Some(failure_category) = telemetry.failure_category {
            self.failure_category = Some(failure_category);
        }
    }

    async fn save(&self, database: &Database) -> Result<(), DatabaseError> {
        sqlx::query(
            "INSERT INTO relay_status (
                relay_url,
                plane,
                account_pubkey,
                last_connect_attempt_at,
                last_connect_success_at,
                last_failure_at,
                failure_category,
                last_notice_reason,
                last_closed_reason,
                last_auth_reason,
                auth_required,
                success_count,
                failure_count,
                latency_ms,
                backoff_until,
                created_at,
                updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(relay_url, plane, account_pubkey) DO UPDATE SET
                last_connect_attempt_at = excluded.last_connect_attempt_at,
                last_connect_success_at = excluded.last_connect_success_at,
                last_failure_at = excluded.last_failure_at,
                failure_category = excluded.failure_category,
                last_notice_reason = excluded.last_notice_reason,
                last_closed_reason = excluded.last_closed_reason,
                last_auth_reason = excluded.last_auth_reason,
                auth_required = excluded.auth_required,
                success_count = excluded.success_count,
                failure_count = excluded.failure_count,
                latency_ms = excluded.latency_ms,
                backoff_until = excluded.backoff_until,
                updated_at = excluded.updated_at",
        )
        .bind(normalize_relay_url(&self.relay_url))
        .bind(self.plane.as_str())
        .bind(serialize_optional_public_key(self.account_pubkey))
        .bind(
            self.last_connect_attempt_at
                .map(|value| value.timestamp_millis()),
        )
        .bind(
            self.last_connect_success_at
                .map(|value| value.timestamp_millis()),
        )
        .bind(self.last_failure_at.map(|value| value.timestamp_millis()))
        .bind(
            self.failure_category
                .map(|category| category.as_str().to_string()),
        )
        .bind(self.last_notice_reason.clone())
        .bind(self.last_closed_reason.clone())
        .bind(self.last_auth_reason.clone())
        .bind(self.auth_required)
        .bind(self.success_count)
        .bind(self.failure_count)
        .bind(self.latency_ms)
        .bind(self.backoff_until.map(|value| value.timestamp_millis()))
        .bind(self.created_at.timestamp_millis())
        .bind(self.updated_at.timestamp_millis())
        .execute(&database.pool)
        .await?;

        Ok(())
    }
}

fn parse_relay_url(value: String) -> Result<RelayUrl, sqlx::Error> {
    RelayUrl::parse(&value).map_err(|error| sqlx::Error::ColumnDecode {
        index: "relay_url".to_string(),
        source: Box::new(error),
    })
}

fn parse_relay_plane(value: String) -> Result<RelayPlane, sqlx::Error> {
    value
        .parse::<RelayPlane>()
        .map_err(|error| create_column_decode_error("plane", &error))
}

fn parse_failure_category(value: String) -> Result<RelayFailureCategory, sqlx::Error> {
    value
        .parse::<RelayFailureCategory>()
        .map_err(|error| create_column_decode_error("failure_category", &error))
}

fn parse_optional_public_key(value: String) -> Result<Option<PublicKey>, sqlx::Error> {
    if value.is_empty() {
        return Ok(None);
    }

    PublicKey::from_hex(&value)
        .map(Some)
        .map_err(|error| create_column_decode_error("account_pubkey", &error.to_string()))
}

fn serialize_optional_public_key(account_pubkey: Option<PublicKey>) -> String {
    account_pubkey
        .map(|pubkey| pubkey.to_hex())
        .unwrap_or_default()
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
            "CREATE TABLE relay_status (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                relay_url TEXT NOT NULL,
                plane TEXT NOT NULL,
                account_pubkey TEXT NOT NULL DEFAULT '',
                last_connect_attempt_at INTEGER,
                last_connect_success_at INTEGER,
                last_failure_at INTEGER,
                failure_category TEXT,
                last_notice_reason TEXT,
                last_closed_reason TEXT,
                last_auth_reason TEXT,
                auth_required INTEGER NOT NULL DEFAULT 0,
                success_count INTEGER NOT NULL DEFAULT 0,
                failure_count INTEGER NOT NULL DEFAULT 0,
                latency_ms INTEGER,
                backoff_until INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(relay_url, plane, account_pubkey)
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
    async fn test_upsert_from_telemetry_records_success_and_failure_state() {
        let database = setup_test_db().await;
        let relay_url = RelayUrl::parse("wss://relay.example.com/").unwrap();
        let account_pubkey =
            PublicKey::from_hex("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap();

        let connected = RelayTelemetry::new(
            RelayTelemetryKind::Connected,
            RelayPlane::Group,
            relay_url.clone(),
        )
        .with_account_pubkey(account_pubkey)
        .with_occurred_at(Utc.with_ymd_and_hms(2026, 3, 7, 9, 0, 0).unwrap());
        let failure = RelayTelemetry::closed(
            RelayPlane::Group,
            relay_url.clone(),
            "blocked by relay policy",
        )
        .with_account_pubkey(account_pubkey)
        .with_occurred_at(Utc.with_ymd_and_hms(2026, 3, 7, 10, 0, 0).unwrap());

        RelayStatusRecord::upsert_from_telemetry(&connected, &database)
            .await
            .unwrap();
        RelayStatusRecord::upsert_from_telemetry(&failure, &database)
            .await
            .unwrap();

        let status = RelayStatusRecord::find(
            &relay_url,
            RelayPlane::Group,
            Some(account_pubkey),
            &database,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            status.relay_url,
            RelayUrl::parse("wss://relay.example.com").unwrap()
        );
        assert_eq!(status.success_count, 1);
        assert_eq!(status.failure_count, 1);
        assert_eq!(
            status.last_connect_success_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 7, 9, 0, 0).unwrap())
        );
        assert_eq!(
            status.last_failure_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 7, 10, 0, 0).unwrap())
        );
        assert_eq!(
            status.failure_category,
            Some(RelayFailureCategory::RelayPolicy)
        );
        assert_eq!(
            status.last_closed_reason.as_deref(),
            Some("blocked by relay policy")
        );
    }

    #[tokio::test]
    async fn test_upsert_from_auth_challenge_sets_auth_required() {
        let database = setup_test_db().await;
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let telemetry = RelayTelemetry::auth_challenge(
            RelayPlane::AccountInbox,
            relay_url.clone(),
            "auth-required: please authenticate",
        )
        .with_occurred_at(Utc.with_ymd_and_hms(2026, 3, 7, 12, 0, 0).unwrap());

        RelayStatusRecord::upsert_from_telemetry(&telemetry, &database)
            .await
            .unwrap();

        let status = RelayStatusRecord::find(&relay_url, RelayPlane::AccountInbox, None, &database)
            .await
            .unwrap()
            .unwrap();

        assert!(status.auth_required);
        assert_eq!(status.failure_count, 1);
        assert_eq!(
            status.failure_category,
            Some(RelayFailureCategory::AuthRequired)
        );
        assert_eq!(
            status.last_auth_reason.as_deref(),
            Some("auth-required: please authenticate")
        );
    }
}
