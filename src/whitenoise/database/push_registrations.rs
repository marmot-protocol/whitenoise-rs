//! Database operations for local push registrations.

use std::str::FromStr;

use chrono::Utc;
use nostr_sdk::{PublicKey, RelayUrl};
use sqlx::SqlitePool;

use crate::perf_instrument;
use crate::whitenoise::push_notifications::{PushPlatform, PushRegistration};

/// Row carrier for `push_registrations` in the per-account DB.
///
/// The `account_pubkey` column no longer exists in the table; the caller
/// supplies it separately and attaches it to the returned
/// [`PushRegistration`].
#[derive(sqlx::FromRow)]
struct LocalPushRegRow {
    platform: String,
    raw_token: String,
    server_pubkey: String,
    relay_hint: Option<String>,
    created_at: i64,
    updated_at: i64,
    last_shared_at: Option<i64>,
}

impl LocalPushRegRow {
    fn into_push_registration(
        self,
        account_pubkey: PublicKey,
    ) -> Result<PushRegistration, sqlx::Error> {
        let platform =
            PushPlatform::from_str(&self.platform).map_err(|error| sqlx::Error::ColumnDecode {
                index: "platform".to_string(),
                source: Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
            })?;

        let server_pubkey =
            PublicKey::parse(&self.server_pubkey).map_err(|error| sqlx::Error::ColumnDecode {
                index: "server_pubkey".to_string(),
                source: Box::new(error),
            })?;

        let relay_hint = self
            .relay_hint
            .map(|value| {
                RelayUrl::parse(&value).map_err(|error| sqlx::Error::ColumnDecode {
                    index: "relay_hint".to_string(),
                    source: Box::new(error),
                })
            })
            .transpose()?;

        let created_at = parse_timestamp_millis(self.created_at)?;
        let updated_at = parse_timestamp_millis(self.updated_at)?;
        let last_shared_at = self
            .last_shared_at
            .map(parse_timestamp_millis)
            .transpose()?;

        Ok(PushRegistration {
            account_pubkey,
            platform,
            raw_token: self.raw_token,
            server_pubkey,
            relay_hint,
            created_at,
            updated_at,
            last_shared_at,
        })
    }
}

fn parse_timestamp_millis(millis: i64) -> Result<chrono::DateTime<chrono::Utc>, sqlx::Error> {
    chrono::DateTime::from_timestamp_millis(millis).ok_or_else(|| sqlx::Error::ColumnDecode {
        index: "timestamp".to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid timestamp millis: {millis}"),
        )),
    })
}

impl PushRegistration {
    /// Return the push registration for this account, if one exists.
    ///
    /// The per-account DB contains at most one row (no WHERE needed).
    #[perf_instrument("db::push_registrations")]
    pub(crate) async fn find(
        account_pubkey: &PublicKey,
        pool: &SqlitePool,
    ) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query_as::<_, LocalPushRegRow>(
            "SELECT platform, raw_token, server_pubkey, relay_hint,
                    created_at, updated_at, last_shared_at
             FROM push_registrations
             WHERE id = 1",
        )
        .fetch_optional(pool)
        .await?;

        match row {
            Some(r) => Ok(Some(r.into_push_registration(*account_pubkey)?)),
            None => Ok(None),
        }
    }

    /// Create or replace the push registration for this account.
    ///
    /// Uses `ON CONFLICT(id)` with a fixed sentinel id (1) to ensure at most
    /// one row per account DB.
    #[perf_instrument("db::push_registrations")]
    pub(crate) async fn upsert(
        account_pubkey: &PublicKey,
        platform: PushPlatform,
        raw_token: &str,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        let now = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalPushRegRow>(
            "INSERT INTO push_registrations (
                 id,
                 platform,
                 raw_token,
                 server_pubkey,
                 relay_hint,
                 created_at,
                 updated_at,
                 last_shared_at
             )
             VALUES (1, ?, ?, ?, ?, ?, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                 platform = excluded.platform,
                 raw_token = excluded.raw_token,
                 server_pubkey = excluded.server_pubkey,
                 relay_hint = excluded.relay_hint,
                 last_shared_at = CASE
                     WHEN push_registrations.platform IS excluded.platform
                      AND push_registrations.raw_token IS excluded.raw_token
                      AND push_registrations.server_pubkey IS excluded.server_pubkey
                      AND push_registrations.relay_hint IS excluded.relay_hint
                     THEN push_registrations.last_shared_at
                     ELSE NULL
                 END,
                 updated_at = CASE
                     WHEN push_registrations.platform IS excluded.platform
                      AND push_registrations.raw_token IS excluded.raw_token
                      AND push_registrations.server_pubkey IS excluded.server_pubkey
                      AND push_registrations.relay_hint IS excluded.relay_hint
                     THEN push_registrations.updated_at
                     ELSE excluded.updated_at
                 END
             RETURNING platform, raw_token, server_pubkey, relay_hint,
                       created_at, updated_at, last_shared_at",
        )
        .bind(platform.as_str())
        .bind(raw_token)
        .bind(server_pubkey.to_hex())
        .bind(relay_hint.map(super::utils::normalize_relay_url))
        .bind(now)
        .bind(now)
        .fetch_one(pool)
        .await?;

        row.into_push_registration(*account_pubkey)
    }

    /// Delete the push registration for this account.
    #[perf_instrument("db::push_registrations")]
    pub(crate) async fn delete(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM push_registrations")
            .execute(pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use nostr_sdk::{Keys, RelayUrl};
    use sqlx::SqlitePool;
    use tempfile::TempDir;

    use super::*;

    async fn setup() -> (SqlitePool, PublicKey, TempDir) {
        let dir = TempDir::new().unwrap();
        let pool = SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("account.db").display()
        ))
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE push_registrations (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                platform        TEXT NOT NULL CHECK (platform IN ('apns', 'fcm')),
                raw_token       TEXT NOT NULL CHECK (
                    length(trim(raw_token, ' ' || char(9) || char(10) || char(13))) > 0
                ),
                server_pubkey   TEXT NOT NULL,
                relay_hint      TEXT,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL,
                last_shared_at  INTEGER
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let pubkey = Keys::generate().public_key();
        (pool, pubkey, dir)
    }

    #[tokio::test]
    async fn test_push_registration_upsert_and_replace() {
        let (pool, account_pubkey, _dir) = setup().await;
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://relay.push.example.com/").unwrap();

        let created = PushRegistration::upsert(
            &account_pubkey,
            PushPlatform::Apns,
            "token-a",
            &first_server,
            Some(&relay_hint),
            &pool,
        )
        .await
        .unwrap();
        assert_eq!(created.platform, PushPlatform::Apns);
        assert_eq!(created.raw_token, "token-a");
        assert_eq!(
            created.relay_hint.as_ref().map(|url| url.as_str()),
            Some("wss://relay.push.example.com")
        );
        assert!(created.last_shared_at.is_none());

        let replaced = PushRegistration::upsert(
            &account_pubkey,
            PushPlatform::Fcm,
            "token-b",
            &second_server,
            None,
            &pool,
        )
        .await
        .unwrap();
        assert_eq!(replaced.platform, PushPlatform::Fcm);
        assert_eq!(replaced.raw_token, "token-b");
        assert_eq!(replaced.server_pubkey, second_server);
        assert_eq!(replaced.relay_hint, None);
        assert_eq!(replaced.created_at, created.created_at);
        assert!(replaced.updated_at >= created.updated_at);

        let stored = PushRegistration::find(&account_pubkey, &pool)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, replaced);
    }

    #[tokio::test]
    async fn test_push_registration_delete() {
        let (pool, account_pubkey, _dir) = setup().await;
        let server_pubkey = Keys::generate().public_key();

        PushRegistration::upsert(
            &account_pubkey,
            PushPlatform::Apns,
            "token-one",
            &server_pubkey,
            None,
            &pool,
        )
        .await
        .unwrap();

        let deleted = PushRegistration::delete(&pool).await.unwrap();
        assert!(deleted);

        let registration = PushRegistration::find(&account_pubkey, &pool)
            .await
            .unwrap();
        assert!(registration.is_none());
    }

    #[tokio::test]
    async fn test_push_registration_upsert_preserves_or_clears_last_shared_at() {
        let (pool, account_pubkey, _dir) = setup().await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://relay.push.example.com").unwrap();

        PushRegistration::upsert(
            &account_pubkey,
            PushPlatform::Apns,
            "token-a",
            &server_pubkey,
            Some(&relay_hint),
            &pool,
        )
        .await
        .unwrap();

        let shared_at_ms = Utc::now().timestamp_millis();
        sqlx::query("UPDATE push_registrations SET last_shared_at = ?")
            .bind(shared_at_ms)
            .execute(&pool)
            .await
            .unwrap();

        let expected_shared_at = DateTime::from_timestamp_millis(shared_at_ms).unwrap();

        let no_op = PushRegistration::upsert(
            &account_pubkey,
            PushPlatform::Apns,
            "token-a",
            &server_pubkey,
            Some(&relay_hint),
            &pool,
        )
        .await
        .unwrap();
        assert_eq!(no_op.last_shared_at, Some(expected_shared_at));

        let changed = PushRegistration::upsert(
            &account_pubkey,
            PushPlatform::Apns,
            "token-b",
            &server_pubkey,
            Some(&relay_hint),
            &pool,
        )
        .await
        .unwrap();
        assert!(changed.last_shared_at.is_none());
    }

    #[tokio::test]
    async fn test_push_registration_upsert_preserves_or_advances_updated_at() {
        let (pool, account_pubkey, _dir) = setup().await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://relay.push.example.com").unwrap();

        PushRegistration::upsert(
            &account_pubkey,
            PushPlatform::Apns,
            "token-a",
            &server_pubkey,
            Some(&relay_hint),
            &pool,
        )
        .await
        .unwrap();

        let updated_at_ms = Utc::now().timestamp_millis() - 60_000;
        sqlx::query("UPDATE push_registrations SET updated_at = ?")
            .bind(updated_at_ms)
            .execute(&pool)
            .await
            .unwrap();

        let expected_updated_at = DateTime::from_timestamp_millis(updated_at_ms).unwrap();

        let no_op = PushRegistration::upsert(
            &account_pubkey,
            PushPlatform::Apns,
            "token-a",
            &server_pubkey,
            Some(&relay_hint),
            &pool,
        )
        .await
        .unwrap();
        assert_eq!(no_op.updated_at, expected_updated_at);

        let changed = PushRegistration::upsert(
            &account_pubkey,
            PushPlatform::Apns,
            "token-b",
            &server_pubkey,
            Some(&relay_hint),
            &pool,
        )
        .await
        .unwrap();
        assert!(changed.updated_at > expected_updated_at);
    }

    #[tokio::test]
    async fn test_push_registration_upsert_rejects_whitespace_only_token_at_db_level() {
        let (pool, account_pubkey, _dir) = setup().await;
        let server_pubkey = Keys::generate().public_key();

        let error = PushRegistration::upsert(
            &account_pubkey,
            PushPlatform::Apns,
            "   ",
            &server_pubkey,
            None,
            &pool,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, sqlx::Error::Database(_)));
    }

    /// Two separate account DBs must be fully isolated: upserting or
    /// deleting in one must not affect the other.
    #[tokio::test]
    async fn test_two_account_dbs_are_isolated() {
        let dir = TempDir::new().unwrap();

        // Account A
        let pool_a = SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("account_a.db").display()
        ))
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE push_registrations (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                platform        TEXT NOT NULL CHECK (platform IN ('apns', 'fcm')),
                raw_token       TEXT NOT NULL,
                server_pubkey   TEXT NOT NULL,
                relay_hint      TEXT,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL,
                last_shared_at  INTEGER
            )",
        )
        .execute(&pool_a)
        .await
        .unwrap();

        // Account B
        let pool_b = SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("account_b.db").display()
        ))
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE push_registrations (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                platform        TEXT NOT NULL CHECK (platform IN ('apns', 'fcm')),
                raw_token       TEXT NOT NULL,
                server_pubkey   TEXT NOT NULL,
                relay_hint      TEXT,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL,
                last_shared_at  INTEGER
            )",
        )
        .execute(&pool_b)
        .await
        .unwrap();

        let pk_a = Keys::generate().public_key();
        let pk_b = Keys::generate().public_key();
        let server = Keys::generate().public_key();

        // Upsert into both
        PushRegistration::upsert(&pk_a, PushPlatform::Apns, "tok-a", &server, None, &pool_a)
            .await
            .unwrap();
        PushRegistration::upsert(&pk_b, PushPlatform::Fcm, "tok-b", &server, None, &pool_b)
            .await
            .unwrap();

        // Delete from A
        PushRegistration::delete(&pool_a).await.unwrap();

        // B is unaffected
        let b_reg = PushRegistration::find(&pk_b, &pool_b).await.unwrap();
        assert!(
            b_reg.is_some(),
            "account B registration must survive A's delete"
        );
        assert_eq!(b_reg.unwrap().raw_token, "tok-b");

        // A is gone
        let a_reg = PushRegistration::find(&pk_a, &pool_a).await.unwrap();
        assert!(a_reg.is_none(), "account A registration must be deleted");
    }
}
