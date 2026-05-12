use chrono::{DateTime, Utc};
use nostr_sdk::EventId;

use super::DatabaseError;
use super::account_db::AccountDatabase;
use super::utils::parse_timestamp;
use crate::perf_instrument;

/// Row structure for the per-account `published_events` table.
///
/// Lives in the per-account SQLite file (post-Phase 18c). The owning account
/// is implicit — it's whichever account owns the file.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct PublishedEvent {
    pub id: i64,
    pub event_id: EventId,
    pub created_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for PublishedEvent
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    i64: sqlx::Decode<'r, <R as sqlx::Row>::Database> + sqlx::Type<<R as sqlx::Row>::Database>,
    String: sqlx::Decode<'r, <R as sqlx::Row>::Database> + sqlx::Type<<R as sqlx::Row>::Database>,
{
    fn from_row(row: &'r R) -> Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let event_id_hex: String = row.try_get("event_id")?;

        let event_id =
            EventId::from_hex(&event_id_hex).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

        let created_at = parse_timestamp(row, "created_at")?;

        Ok(PublishedEvent {
            id,
            event_id,
            created_at,
        })
    }
}

impl PublishedEvent {
    /// Records that this account published a specific event.
    #[perf_instrument("db::published_events")]
    pub(crate) async fn create(
        event_id: &EventId,
        db: &AccountDatabase,
    ) -> Result<(), DatabaseError> {
        sqlx::query("INSERT OR IGNORE INTO published_events (event_id) VALUES (?)")
            .bind(event_id.to_hex())
            .execute(&db.inner.pool)
            .await?;

        tracing::debug!(
            target: "whitenoise::database::published_events",
            "Recorded published event {} for account {}",
            event_id.to_hex(),
            db.account_pubkey().to_hex()
        );

        Ok(())
    }

    /// Checks if this account published a specific event.
    #[perf_instrument("db::published_events")]
    pub(crate) async fn exists(
        event_id: &EventId,
        db: &AccountDatabase,
    ) -> Result<bool, DatabaseError> {
        let result: Option<(i64,)> =
            sqlx::query_as("SELECT EXISTS(SELECT 1 FROM published_events WHERE event_id = ?)")
                .bind(event_id.to_hex())
                .fetch_optional(&db.inner.pool)
                .await?;

        Ok(result.is_some_and(|row| row.0 != 0))
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::{EventId, Keys};
    use std::str::FromStr;
    use tempfile::TempDir;

    use super::*;

    async fn setup() -> (AccountDatabase, TempDir) {
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let path = dir.path().join("acct.db");
        let db = AccountDatabase::new(pubkey, path).await.unwrap();

        sqlx::query("DROP TABLE IF EXISTS published_events")
            .execute(&db.inner.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE published_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL UNIQUE
                    CHECK (length(event_id) = 64 AND event_id GLOB '[0-9a-fA-F]*'),
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&db.inner.pool)
        .await
        .unwrap();

        (db, dir)
    }

    fn create_test_event_id() -> EventId {
        let keys = Keys::generate();
        EventId::from_str(&keys.public_key().to_string()).unwrap_or_else(|_| {
            EventId::from_hex("1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
                .unwrap()
        })
    }

    #[tokio::test]
    async fn test_create_and_exists() {
        let (db, _dir) = setup().await;
        let event_id = create_test_event_id();

        assert!(!PublishedEvent::exists(&event_id, &db).await.unwrap());
        PublishedEvent::create(&event_id, &db).await.unwrap();
        assert!(PublishedEvent::exists(&event_id, &db).await.unwrap());
    }

    #[tokio::test]
    async fn test_create_duplicate_ignored() {
        let (db, _dir) = setup().await;
        let event_id = create_test_event_id();

        PublishedEvent::create(&event_id, &db).await.unwrap();
        PublishedEvent::create(&event_id, &db).await.unwrap();

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM published_events WHERE event_id = ?")
                .bind(event_id.to_hex())
                .fetch_one(&db.inner.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1);
    }
}
