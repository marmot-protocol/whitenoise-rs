use chrono::{DateTime, Utc};
use nostr_sdk::{EventId, Kind, PublicKey};

use super::{Database, DatabaseError, account_db::AccountDatabase, utils::parse_timestamp};
use crate::perf_instrument;
use crate::whitenoise::{error::WhitenoiseError, relays::RelayType};

/// Row structure for `processed_events`.
///
/// After Phase 18c, account-scoped rows live in per-account DB files (no
/// `account_pubkey` column there — the file is the scope) and global-scoped
/// rows continue to live in shared `processed_events` (with
/// `account_pubkey IS NULL`). The struct field is `Option<PublicKey>` for the
/// global path's API only — the per-account ops never populate it.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ProcessedEvent {
    pub id: i64,
    pub event_id: EventId,
    pub account_pubkey: Option<PublicKey>,
    pub created_at: DateTime<Utc>,
    pub event_created_at: Option<DateTime<Utc>>,
    pub event_kind: Option<Kind>,
    pub author: Option<PublicKey>,
}

impl<'r, R> sqlx::FromRow<'r, R> for ProcessedEvent
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    i64: sqlx::Decode<'r, <R as sqlx::Row>::Database> + sqlx::Type<<R as sqlx::Row>::Database>,
    String: sqlx::Decode<'r, <R as sqlx::Row>::Database> + sqlx::Type<<R as sqlx::Row>::Database>,
{
    fn from_row(row: &'r R) -> Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let event_id_hex: String = row.try_get("event_id")?;

        // `account_pubkey` is only present in the shared schema. Per-account
        // rows decode it as `None`.
        let account_pubkey_hex: Option<String> = row.try_get("account_pubkey").unwrap_or(None);

        let event_id =
            EventId::from_hex(&event_id_hex).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

        let account_pubkey = match account_pubkey_hex {
            Some(hex) => {
                Some(PublicKey::from_hex(&hex).map_err(|e| sqlx::Error::Decode(Box::new(e)))?)
            }
            None => None,
        };

        let created_at = parse_timestamp(row, "created_at")?;

        let event_created_at = match row.try_get::<Option<i64>, &str>("event_created_at")? {
            Some(timestamp_ms) => Some(DateTime::from_timestamp_millis(timestamp_ms).ok_or_else(
                || {
                    sqlx::Error::Decode(
                        format!("Invalid event_created_at timestamp value: {}", timestamp_ms)
                            .into(),
                    )
                },
            )?),
            None => None,
        };

        let event_kind = row
            .try_get::<Option<i64>, &str>("event_kind")?
            .map(|kind| Kind::from(kind as u16));

        let author = match row.try_get::<Option<String>, &str>("author")? {
            Some(author_hex) => Some(PublicKey::from_hex(&author_hex).map_err(|e| {
                sqlx::Error::Decode(format!("Invalid author public key: {}", e).into())
            })?),
            None => None,
        };

        Ok(ProcessedEvent {
            id,
            event_id,
            account_pubkey,
            created_at,
            event_created_at,
            event_kind,
            author,
        })
    }
}

impl ProcessedEvent {
    // ── Global-scoped ops (shared DB) ───────────────────────────────────

    /// Records a global-scoped processed event (`account_pubkey IS NULL`) in
    /// the shared DB.
    #[perf_instrument("db::processed_events")]
    pub(crate) async fn create_global(
        event_id: &EventId,
        event_created_at: Option<DateTime<Utc>>,
        event_kind: Option<Kind>,
        author: Option<&PublicKey>,
        database: &Database,
    ) -> Result<(), DatabaseError> {
        let event_timestamp_ms = event_created_at.map(|dt| dt.timestamp_millis());
        let event_kind_i64 = event_kind.map(|k| k.as_u16() as i64);
        let author_hex = author.map(|pk| pk.to_hex());

        sqlx::query(
            "INSERT OR IGNORE INTO processed_events \
                (event_id, account_pubkey, event_created_at, event_kind, author) \
             VALUES (?, NULL, ?, ?, ?)",
        )
        .bind(event_id.to_hex())
        .bind(event_timestamp_ms)
        .bind(event_kind_i64)
        .bind(author_hex)
        .execute(&database.pool)
        .await?;

        Ok(())
    }

    /// Checks if a global-scoped event was processed (shared DB).
    #[perf_instrument("db::processed_events")]
    pub(crate) async fn exists_global(
        event_id: &EventId,
        database: &Database,
    ) -> Result<bool, DatabaseError> {
        let result: Option<(bool,)> = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM processed_events \
             WHERE event_id = ? AND account_pubkey IS NULL)",
        )
        .bind(event_id.to_hex())
        .fetch_optional(&database.pool)
        .await?;

        Ok(result.map(|(exists,)| exists).unwrap_or(false))
    }

    /// Newest event timestamp for a kind in the shared (global-scope) table,
    /// optionally filtered by author.
    #[perf_instrument("db::processed_events")]
    pub(crate) async fn newest_global_event_timestamp_for_kind(
        event_kind: u16,
        author_pubkey: Option<&PublicKey>,
        database: &Database,
    ) -> Result<Option<DateTime<Utc>>, DatabaseError> {
        let result: Option<Option<i64>> = match author_pubkey {
            Some(author) => {
                sqlx::query_scalar(
                    "SELECT MAX(event_created_at) FROM processed_events \
                 WHERE account_pubkey IS NULL AND event_kind = ? AND author = ?",
                )
                .bind(event_kind as i64)
                .bind(author.to_hex())
                .fetch_optional(&database.pool)
                .await?
            }
            None => {
                sqlx::query_scalar(
                    "SELECT MAX(event_created_at) FROM processed_events \
                 WHERE account_pubkey IS NULL AND event_kind = ?",
                )
                .bind(event_kind as i64)
                .fetch_optional(&database.pool)
                .await?
            }
        };

        Ok(result.flatten().and_then(DateTime::from_timestamp_millis))
    }

    /// Newest relay-list event timestamp in the shared table for a user.
    /// (Relay-list events are global-scoped — see `handle_relay_list`.)
    #[perf_instrument("db::processed_events")]
    pub(crate) async fn newest_relay_event_timestamp(
        user_pubkey: &PublicKey,
        relay_type: RelayType,
        database: &Database,
    ) -> Result<Option<DateTime<Utc>>, WhitenoiseError> {
        let kind = match relay_type {
            RelayType::Nip65 => 10002,
            RelayType::Inbox => 10050,
            RelayType::KeyPackage => 10051,
        };

        Self::newest_global_event_timestamp_for_kind(kind, Some(user_pubkey), database)
            .await
            .map_err(WhitenoiseError::Database)
    }

    // ── Account-scoped ops (per-account DB) ─────────────────────────────

    /// Records an account-scoped processed event in this account's per-account DB.
    #[perf_instrument("db::processed_events")]
    pub(crate) async fn create_for_account(
        event_id: &EventId,
        event_created_at: Option<DateTime<Utc>>,
        event_kind: Option<Kind>,
        author: Option<&PublicKey>,
        db: &AccountDatabase,
    ) -> Result<(), DatabaseError> {
        let event_timestamp_ms = event_created_at.map(|dt| dt.timestamp_millis());
        let event_kind_i64 = event_kind.map(|k| k.as_u16() as i64);
        let author_hex = author.map(|pk| pk.to_hex());

        sqlx::query(
            "INSERT OR IGNORE INTO processed_events \
                (event_id, event_created_at, event_kind, author) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(event_id.to_hex())
        .bind(event_timestamp_ms)
        .bind(event_kind_i64)
        .bind(author_hex)
        .execute(&db.inner.pool)
        .await?;

        Ok(())
    }

    /// Checks if an account-scoped event was processed in this account's DB.
    #[perf_instrument("db::processed_events")]
    pub(crate) async fn exists_for_account(
        event_id: &EventId,
        db: &AccountDatabase,
    ) -> Result<bool, DatabaseError> {
        let result: Option<(bool,)> =
            sqlx::query_as("SELECT EXISTS(SELECT 1 FROM processed_events WHERE event_id = ?)")
                .bind(event_id.to_hex())
                .fetch_optional(&db.inner.pool)
                .await?;

        Ok(result.map(|(exists,)| exists).unwrap_or(false))
    }

    /// Newest account-scoped contact-list event timestamp.
    #[perf_instrument("db::processed_events")]
    pub(crate) async fn newest_contact_list_timestamp(
        db: &AccountDatabase,
    ) -> Result<Option<DateTime<Utc>>, WhitenoiseError> {
        let result: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT MAX(event_created_at) FROM processed_events WHERE event_kind = 3",
        )
        .fetch_optional(&db.inner.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(result.flatten().and_then(DateTime::from_timestamp_millis))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use nostr_sdk::{EventId, Keys};
    use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
    use std::str::FromStr;
    use tempfile::TempDir;

    use super::*;

    /// Shared test DB: schema for global-scoped rows.
    async fn setup_shared_db() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE processed_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL,
                account_pubkey TEXT,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                event_created_at INTEGER DEFAULT NULL,
                event_kind INTEGER DEFAULT NULL,
                author TEXT DEFAULT NULL,
                UNIQUE(event_id, account_pubkey)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    fn create_test_event_id() -> EventId {
        let keys = Keys::generate();
        EventId::from_str(&keys.public_key().to_string()).unwrap_or_else(|_| {
            EventId::from_hex("1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
                .unwrap()
        })
    }

    fn wrap_pool_in_database(pool: SqlitePool) -> Database {
        Database {
            pool,
            path: std::path::PathBuf::from(":memory:"),
            last_connected: std::time::SystemTime::now(),
        }
    }

    async fn setup_account_db() -> (AccountDatabase, TempDir) {
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let path = dir.path().join("acct.db");
        let db = AccountDatabase::new(pubkey, path).await.unwrap();
        sqlx::query("DROP TABLE IF EXISTS processed_events")
            .execute(&db.inner.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE processed_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL UNIQUE,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                event_created_at INTEGER DEFAULT NULL,
                event_kind INTEGER DEFAULT NULL,
                author TEXT DEFAULT NULL
            )",
        )
        .execute(&db.inner.pool)
        .await
        .unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn test_global_create_and_exists() {
        let database = wrap_pool_in_database(setup_shared_db().await);
        let event_id = create_test_event_id();

        assert!(
            !ProcessedEvent::exists_global(&event_id, &database)
                .await
                .unwrap()
        );
        ProcessedEvent::create_global(
            &event_id,
            Some(Utc::now()),
            Some(Kind::RelayList),
            None,
            &database,
        )
        .await
        .unwrap();
        assert!(
            ProcessedEvent::exists_global(&event_id, &database)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_account_create_and_exists() {
        let (db, _dir) = setup_account_db().await;
        let event_id = create_test_event_id();

        assert!(
            !ProcessedEvent::exists_for_account(&event_id, &db)
                .await
                .unwrap()
        );
        ProcessedEvent::create_for_account(
            &event_id,
            Some(Utc::now()),
            Some(Kind::TextNote),
            None,
            &db,
        )
        .await
        .unwrap();
        assert!(
            ProcessedEvent::exists_for_account(&event_id, &db)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_newest_contact_list_timestamp() {
        let (db, _dir) = setup_account_db().await;
        let event_id = create_test_event_id();
        let timestamp = Utc::now();

        ProcessedEvent::create_for_account(
            &event_id,
            Some(timestamp),
            Some(Kind::ContactList),
            None,
            &db,
        )
        .await
        .unwrap();

        let result = ProcessedEvent::newest_contact_list_timestamp(&db)
            .await
            .unwrap();
        assert_eq!(
            result.unwrap().timestamp_millis(),
            timestamp.timestamp_millis()
        );
    }

    #[tokio::test]
    async fn test_newest_relay_event_timestamp() {
        let database = wrap_pool_in_database(setup_shared_db().await);
        let event_id = create_test_event_id();
        let user_pubkey = Keys::generate().public_key();
        let timestamp = Utc::now();

        ProcessedEvent::create_global(
            &event_id,
            Some(timestamp),
            Some(Kind::RelayList),
            Some(&user_pubkey),
            &database,
        )
        .await
        .unwrap();

        let result =
            ProcessedEvent::newest_relay_event_timestamp(&user_pubkey, RelayType::Nip65, &database)
                .await
                .unwrap();
        assert_eq!(
            result.unwrap().timestamp_millis(),
            timestamp.timestamp_millis()
        );

        let result =
            ProcessedEvent::newest_relay_event_timestamp(&user_pubkey, RelayType::Inbox, &database)
                .await
                .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_newest_global_event_timestamp_for_kind_with_author_filter() {
        let database = wrap_pool_in_database(setup_shared_db().await);
        let event_id1 = create_test_event_id();
        let event_id2 = create_test_event_id();

        let author1 = Keys::generate().public_key();
        let author2 = Keys::generate().public_key();

        let older = Utc::now() - chrono::Duration::minutes(10);
        let newer = Utc::now();

        ProcessedEvent::create_global(
            &event_id1,
            Some(older),
            Some(Kind::Metadata),
            Some(&author1),
            &database,
        )
        .await
        .unwrap();
        ProcessedEvent::create_global(
            &event_id2,
            Some(newer),
            Some(Kind::Metadata),
            Some(&author2),
            &database,
        )
        .await
        .unwrap();

        let result =
            ProcessedEvent::newest_global_event_timestamp_for_kind(0, Some(&author1), &database)
                .await
                .unwrap();
        assert_eq!(result.unwrap().timestamp_millis(), older.timestamp_millis());

        let result = ProcessedEvent::newest_global_event_timestamp_for_kind(0, None, &database)
            .await
            .unwrap();
        assert_eq!(result.unwrap().timestamp_millis(), newer.timestamp_millis());

        let unknown = Keys::generate().public_key();
        let result =
            ProcessedEvent::newest_global_event_timestamp_for_kind(0, Some(&unknown), &database)
                .await
                .unwrap();
        assert!(result.is_none());
    }
}
