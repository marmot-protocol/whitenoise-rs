//! Database operations for local push registrations.

use core::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use nostr_sdk::{PublicKey, RelayUrl};

use super::{
    Database,
    utils::{parse_optional_timestamp, parse_timestamp},
};
use crate::perf_instrument;
use crate::whitenoise::push_notifications::{PushPlatform, PushRegistration};

struct PushRegistrationRow {
    account_pubkey: PublicKey,
    platform: PushPlatform,
    raw_token: String,
    server_pubkey: PublicKey,
    relay_hint: Option<RelayUrl>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_shared_at: Option<DateTime<Utc>>,
}

impl fmt::Debug for PushRegistrationRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushRegistrationRow")
            .field("account_pubkey", &self.account_pubkey)
            .field("platform", &self.platform)
            .field("raw_token", &"<redacted>")
            .field("server_pubkey", &self.server_pubkey)
            .field("relay_hint", &self.relay_hint)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("last_shared_at", &self.last_shared_at)
            .finish()
    }
}

impl<'r, R> sqlx::FromRow<'r, R> for PushRegistrationRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<String>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<i64>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> Result<Self, sqlx::Error> {
        let account_pubkey_str: String = row.try_get("account_pubkey")?;
        let account_pubkey =
            PublicKey::parse(&account_pubkey_str).map_err(|error| sqlx::Error::ColumnDecode {
                index: "account_pubkey".to_string(),
                source: Box::new(error),
            })?;

        let platform_str: String = row.try_get("platform")?;
        let platform =
            PushPlatform::from_str(&platform_str).map_err(|error| sqlx::Error::ColumnDecode {
                index: "platform".to_string(),
                source: Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
            })?;

        let raw_token: String = row.try_get("raw_token")?;

        let server_pubkey_str: String = row.try_get("server_pubkey")?;
        let server_pubkey =
            PublicKey::parse(&server_pubkey_str).map_err(|error| sqlx::Error::ColumnDecode {
                index: "server_pubkey".to_string(),
                source: Box::new(error),
            })?;

        let relay_hint_str: Option<String> = row.try_get("relay_hint")?;
        let relay_hint = relay_hint_str
            .map(|value| {
                RelayUrl::parse(&value).map_err(|error| sqlx::Error::ColumnDecode {
                    index: "relay_hint".to_string(),
                    source: Box::new(error),
                })
            })
            .transpose()?;

        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;
        let last_shared_at = parse_optional_timestamp(row, "last_shared_at")?;

        Ok(Self {
            account_pubkey,
            platform,
            raw_token,
            server_pubkey,
            relay_hint,
            created_at,
            updated_at,
            last_shared_at,
        })
    }
}

impl From<PushRegistrationRow> for PushRegistration {
    fn from(row: PushRegistrationRow) -> Self {
        Self {
            account_pubkey: row.account_pubkey,
            platform: row.platform,
            raw_token: row.raw_token,
            server_pubkey: row.server_pubkey,
            relay_hint: row.relay_hint,
            created_at: row.created_at,
            updated_at: row.updated_at,
            last_shared_at: row.last_shared_at,
        }
    }
}

impl PushRegistration {
    #[perf_instrument("db::push_registrations")]
    pub(crate) async fn find_by_account_pubkey(
        account_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query_as::<_, PushRegistrationRow>(
            "SELECT *
             FROM push_registrations
             WHERE account_pubkey = ?",
        )
        .bind(account_pubkey.to_hex())
        .fetch_optional(&database.pool)
        .await?;

        Ok(row.map(Into::into))
    }

    #[perf_instrument("db::push_registrations")]
    pub(crate) async fn upsert(
        account_pubkey: &PublicKey,
        platform: PushPlatform,
        raw_token: &str,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
        database: &Database,
    ) -> Result<Self, sqlx::Error> {
        let now = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, PushRegistrationRow>(
            "INSERT INTO push_registrations (
                 account_pubkey,
                 platform,
                 raw_token,
                 server_pubkey,
                 relay_hint,
                 created_at,
                 updated_at,
                 last_shared_at
             )
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL)
             ON CONFLICT(account_pubkey) DO UPDATE SET
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
             RETURNING *",
        )
        .bind(account_pubkey.to_hex())
        .bind(platform.as_str())
        .bind(raw_token)
        .bind(server_pubkey.to_hex())
        .bind(relay_hint.map(super::utils::normalize_relay_url))
        .bind(now)
        .bind(now)
        .fetch_one(&database.pool)
        .await?;

        Ok(row.into())
    }

    #[perf_instrument("db::push_registrations")]
    pub(crate) async fn delete_by_account_pubkey(
        account_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM push_registrations
             WHERE account_pubkey = ?",
        )
        .bind(account_pubkey.to_hex())
        .execute(&database.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use nostr_sdk::{Keys, RelayUrl};

    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[tokio::test]
    async fn test_push_registration_upsert_and_replace() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://relay.push.example.com/").unwrap();

        let created = PushRegistration::upsert(
            &account.pubkey,
            PushPlatform::Apns,
            "token-a",
            &first_server,
            Some(&relay_hint),
            &whitenoise.database,
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
            &account.pubkey,
            PushPlatform::Fcm,
            "token-b",
            &second_server,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(replaced.platform, PushPlatform::Fcm);
        assert_eq!(replaced.raw_token, "token-b");
        assert_eq!(replaced.server_pubkey, second_server);
        assert_eq!(replaced.relay_hint, None);
        assert_eq!(replaced.created_at, created.created_at);
        assert!(replaced.updated_at >= created.updated_at);

        let stored =
            PushRegistration::find_by_account_pubkey(&account.pubkey, &whitenoise.database)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(stored, replaced);
    }

    #[tokio::test]
    async fn test_push_registration_delete_and_account_scoping() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account_one = whitenoise.create_identity().await.unwrap();
        let account_two = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();

        PushRegistration::upsert(
            &account_one.pubkey,
            PushPlatform::Apns,
            "token-one",
            &server_pubkey,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();
        PushRegistration::upsert(
            &account_two.pubkey,
            PushPlatform::Fcm,
            "token-two",
            &server_pubkey,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let deleted =
            PushRegistration::delete_by_account_pubkey(&account_one.pubkey, &whitenoise.database)
                .await
                .unwrap();
        assert!(deleted);

        let account_one_registration =
            PushRegistration::find_by_account_pubkey(&account_one.pubkey, &whitenoise.database)
                .await
                .unwrap();
        let account_two_registration =
            PushRegistration::find_by_account_pubkey(&account_two.pubkey, &whitenoise.database)
                .await
                .unwrap();

        assert!(account_one_registration.is_none());
        assert_eq!(account_two_registration.unwrap().raw_token, "token-two");
    }

    #[tokio::test]
    async fn test_push_registration_upsert_preserves_or_clears_last_shared_at() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://relay.push.example.com").unwrap();

        PushRegistration::upsert(
            &account.pubkey,
            PushPlatform::Apns,
            "token-a",
            &server_pubkey,
            Some(&relay_hint),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let shared_at_ms = Utc::now().timestamp_millis();
        sqlx::query(
            "UPDATE push_registrations
             SET last_shared_at = ?
             WHERE account_pubkey = ?",
        )
        .bind(shared_at_ms)
        .bind(account.pubkey.to_hex())
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        let expected_shared_at = DateTime::from_timestamp_millis(shared_at_ms).unwrap();

        let no_op = PushRegistration::upsert(
            &account.pubkey,
            PushPlatform::Apns,
            "token-a",
            &server_pubkey,
            Some(&relay_hint),
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(no_op.last_shared_at, Some(expected_shared_at));

        let changed = PushRegistration::upsert(
            &account.pubkey,
            PushPlatform::Apns,
            "token-b",
            &server_pubkey,
            Some(&relay_hint),
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(changed.last_shared_at.is_none());
    }

    #[tokio::test]
    async fn test_push_registration_upsert_preserves_or_advances_updated_at() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://relay.push.example.com").unwrap();

        PushRegistration::upsert(
            &account.pubkey,
            PushPlatform::Apns,
            "token-a",
            &server_pubkey,
            Some(&relay_hint),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let updated_at_ms = Utc::now().timestamp_millis() - 60_000;
        sqlx::query(
            "UPDATE push_registrations
             SET updated_at = ?
             WHERE account_pubkey = ?",
        )
        .bind(updated_at_ms)
        .bind(account.pubkey.to_hex())
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        let expected_updated_at = DateTime::from_timestamp_millis(updated_at_ms).unwrap();

        let no_op = PushRegistration::upsert(
            &account.pubkey,
            PushPlatform::Apns,
            "token-a",
            &server_pubkey,
            Some(&relay_hint),
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(no_op.updated_at, expected_updated_at);

        let changed = PushRegistration::upsert(
            &account.pubkey,
            PushPlatform::Apns,
            "token-b",
            &server_pubkey,
            Some(&relay_hint),
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(changed.updated_at > expected_updated_at);
    }

    #[tokio::test]
    async fn test_push_registration_upsert_rejects_whitespace_only_token_at_db_level() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();

        let error = PushRegistration::upsert(
            &account.pubkey,
            PushPlatform::Apns,
            "   ",
            &server_pubkey,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, sqlx::Error::Database(_)));
    }

    #[test]
    fn test_push_registration_row_debug_redacts_raw_token() {
        let row = PushRegistrationRow {
            account_pubkey: Keys::generate().public_key(),
            platform: PushPlatform::Apns,
            raw_token: "super-secret-token".to_string(),
            server_pubkey: Keys::generate().public_key(),
            relay_hint: Some(RelayUrl::parse("wss://push.example.com").unwrap()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_shared_at: None,
        };

        let debug_output = format!("{row:?}");

        assert!(debug_output.contains("<redacted>"));
        assert!(!debug_output.contains("super-secret-token"));
    }
}
