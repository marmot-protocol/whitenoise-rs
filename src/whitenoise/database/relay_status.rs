use chrono::{DateTime, Utc};
use nostr_sdk::{PublicKey, RelayUrl};

use super::{
    Database, DatabaseError,
    utils::{
        normalize_relay_url, parse_failure_category, parse_optional_public_key,
        parse_optional_timestamp, parse_relay_plane, parse_relay_url, parse_timestamp,
        serialize_optional_public_key,
    },
};
use crate::perf_instrument;
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RelayStatusLookupKey {
    pub(crate) relay_url: RelayUrl,
    pub(crate) plane: RelayPlane,
    pub(crate) account_pubkey: Option<PublicKey>,
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
            parse_optional_public_key(row.try_get::<Option<String>, _>("account_pubkey")?)?;
        let last_connect_attempt_at = parse_optional_timestamp(row, "last_connect_attempt_at")?;
        let last_connect_success_at = parse_optional_timestamp(row, "last_connect_success_at")?;
        let last_failure_at = parse_optional_timestamp(row, "last_failure_at")?;
        let failure_category = row
            .try_get::<Option<String>, _>("failure_category")?
            .map(parse_failure_category)
            .transpose()?;
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
    pub(crate) fn lookup_key(&self) -> RelayStatusLookupKey {
        RelayStatusLookupKey {
            relay_url: self.relay_url.clone(),
            plane: self.plane,
            account_pubkey: self.account_pubkey,
        }
    }

    #[allow(dead_code)]
    #[perf_instrument("db::relay_status")]
    pub(crate) async fn find(
        relay_url: &RelayUrl,
        plane: RelayPlane,
        account_pubkey: Option<PublicKey>,
        database: &Database,
    ) -> Result<Option<Self>, DatabaseError> {
        let record = match account_pubkey {
            Some(account_pubkey) => {
                sqlx::query_as::<_, Self>(
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
                .bind(account_pubkey.to_hex())
                .fetch_optional(&database.pool)
                .await?
            }
            None => {
                sqlx::query_as::<_, Self>(
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
                     WHERE relay_url = ? AND plane = ? AND account_pubkey IS NULL",
                )
                .bind(normalize_relay_url(relay_url))
                .bind(plane.as_str())
                .fetch_optional(&database.pool)
                .await?
            }
        };

        Ok(record)
    }

    #[perf_instrument("db::relay_status")]
    pub(crate) async fn find_many(
        lookup_keys: &[RelayStatusLookupKey],
        database: &Database,
    ) -> Result<Vec<Self>, DatabaseError> {
        if lookup_keys.is_empty() {
            return Ok(Vec::new());
        }

        let mut query_builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
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
             WHERE ",
        );

        for (index, lookup_key) in lookup_keys.iter().enumerate() {
            if index > 0 {
                query_builder.push(" OR ");
            }

            query_builder.push("(");
            query_builder.push("relay_url = ");
            query_builder.push_bind(normalize_relay_url(&lookup_key.relay_url));
            query_builder.push(" AND plane = ");
            query_builder.push_bind(lookup_key.plane.as_str());

            match lookup_key.account_pubkey {
                Some(account_pubkey) => {
                    query_builder.push(" AND account_pubkey = ");
                    query_builder.push_bind(account_pubkey.to_hex());
                }
                None => {
                    query_builder.push(" AND account_pubkey IS NULL");
                }
            }

            query_builder.push(")");
        }

        let records = query_builder
            .build_query_as::<Self>()
            .fetch_all(&database.pool)
            .await?;

        Ok(records)
    }

    #[perf_instrument("db::relay_status")]
    pub(crate) async fn upsert_from_telemetry(
        telemetry: &RelayTelemetry,
        database: &Database,
    ) -> Result<(), DatabaseError> {
        // Compute per-event deltas rather than read-modify-write, so two concurrent
        // persistors cannot race and both write `success_count = 1` from a read of 0.
        let (success_delta, failure_delta) = match telemetry.kind {
            RelayTelemetryKind::Connected
            | RelayTelemetryKind::PublishSuccess
            | RelayTelemetryKind::QuerySuccess
            | RelayTelemetryKind::SubscriptionSuccess => (1i64, 0i64),
            RelayTelemetryKind::Disconnected
            | RelayTelemetryKind::Notice
            | RelayTelemetryKind::Closed
            | RelayTelemetryKind::AuthChallenge
            | RelayTelemetryKind::PublishFailure
            | RelayTelemetryKind::QueryFailure
            | RelayTelemetryKind::SubscriptionFailure => (0i64, 1i64),
            RelayTelemetryKind::PublishAttempt
            | RelayTelemetryKind::QueryAttempt
            | RelayTelemetryKind::SubscriptionAttempt => (0i64, 0i64),
        };

        let connect_attempt_at = match telemetry.kind {
            RelayTelemetryKind::Connected | RelayTelemetryKind::Disconnected => {
                Some(telemetry.occurred_at.timestamp_millis())
            }
            _ => None,
        };

        let connect_success_at = if telemetry.kind == RelayTelemetryKind::Connected {
            Some(telemetry.occurred_at.timestamp_millis())
        } else {
            None
        };

        let failure_at = if failure_delta > 0 {
            Some(telemetry.occurred_at.timestamp_millis())
        } else {
            None
        };

        let is_auth = matches!(
            telemetry.failure_category,
            Some(RelayFailureCategory::AuthRequired | RelayFailureCategory::AuthFailed)
        ) || telemetry.kind == RelayTelemetryKind::AuthChallenge;

        let notice_reason = if telemetry.kind == RelayTelemetryKind::Notice {
            telemetry.message.clone()
        } else {
            None
        };
        let closed_reason = if telemetry.kind == RelayTelemetryKind::Closed {
            telemetry.message.clone()
        } else {
            None
        };
        let auth_reason = if telemetry.kind == RelayTelemetryKind::AuthChallenge {
            telemetry.message.clone()
        } else {
            None
        };
        let failure_category = telemetry.failure_category.map(|c| c.as_str().to_string());
        let now = telemetry.occurred_at.timestamp_millis();

        sqlx::query(
            "INSERT INTO relay_status (
                relay_url, plane, account_pubkey,
                last_connect_attempt_at, last_connect_success_at, last_failure_at,
                failure_category, last_notice_reason, last_closed_reason, last_auth_reason,
                auth_required, success_count, failure_count, latency_ms, backoff_until,
                created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)
            ON CONFLICT DO UPDATE SET
                last_connect_attempt_at = CASE WHEN excluded.last_connect_attempt_at IS NOT NULL
                    THEN excluded.last_connect_attempt_at
                    ELSE relay_status.last_connect_attempt_at END,
                last_connect_success_at = CASE WHEN excluded.last_connect_success_at IS NOT NULL
                    THEN excluded.last_connect_success_at
                    ELSE relay_status.last_connect_success_at END,
                last_failure_at = CASE WHEN excluded.last_failure_at IS NOT NULL
                    THEN excluded.last_failure_at
                    ELSE relay_status.last_failure_at END,
                failure_category = CASE WHEN excluded.failure_category IS NOT NULL
                    THEN excluded.failure_category
                    ELSE relay_status.failure_category END,
                last_notice_reason = CASE WHEN excluded.last_notice_reason IS NOT NULL
                    THEN excluded.last_notice_reason
                    ELSE relay_status.last_notice_reason END,
                last_closed_reason = CASE WHEN excluded.last_closed_reason IS NOT NULL
                    THEN excluded.last_closed_reason
                    ELSE relay_status.last_closed_reason END,
                last_auth_reason = CASE WHEN excluded.last_auth_reason IS NOT NULL
                    THEN excluded.last_auth_reason
                    ELSE relay_status.last_auth_reason END,
                auth_required = MAX(relay_status.auth_required, excluded.auth_required),
                success_count = relay_status.success_count + excluded.success_count,
                failure_count = relay_status.failure_count + excluded.failure_count,
                updated_at = excluded.updated_at",
        )
        .bind(normalize_relay_url(&telemetry.relay_url))
        .bind(telemetry.plane.as_str())
        .bind(serialize_optional_public_key(telemetry.account_pubkey))
        .bind(connect_attempt_at)
        .bind(connect_success_at)
        .bind(failure_at)
        .bind(failure_category)
        .bind(notice_reason)
        .bind(closed_reason)
        .bind(auth_reason)
        .bind(is_auth)
        .bind(success_delta)
        .bind(failure_delta)
        .bind(now) // created_at
        .bind(now) // updated_at
        .execute(&database.pool)
        .await?;

        Ok(())
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
            "CREATE TABLE relay_status (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                relay_url TEXT NOT NULL,
                plane TEXT NOT NULL,
                account_pubkey TEXT,
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

        sqlx::query(
            "CREATE UNIQUE INDEX idx_relay_status_global_unique
             ON relay_status(relay_url, plane)
             WHERE account_pubkey IS NULL",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE UNIQUE INDEX idx_relay_status_account_unique
             ON relay_status(relay_url, plane, account_pubkey)
             WHERE account_pubkey IS NOT NULL",
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
