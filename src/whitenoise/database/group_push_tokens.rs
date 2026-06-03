//! Database operations for cached per-group push tokens.

use std::collections::BTreeMap;

use crate::marmot::GroupId;
use chrono::Utc;
use nostr_sdk::{PublicKey, RelayUrl};
use sqlx::SqlitePool;

use crate::marmot::push::LeafTokenTag;
use crate::perf_instrument;
use crate::whitenoise::push_notifications::{GroupPushToken, PushPlatform};

/// Row carrier for `group_push_tokens` in the per-account DB.
///
/// The `account_pubkey` column no longer exists in the table; the caller
/// supplies it separately and attaches it to the returned
/// [`GroupPushToken`].
#[derive(sqlx::FromRow)]
struct LocalGroupPushTokenRow {
    mls_group_id: Vec<u8>,
    member_pubkey: String,
    leaf_index: i64,
    platform: Option<String>,
    token_fingerprint: Option<String>,
    server_pubkey: String,
    relay_hint: Option<String>,
    encrypted_token: String,
    created_at: i64,
    updated_at: i64,
}

pub(crate) struct GroupPushTokenUpsert<'a> {
    pub(crate) mls_group_id: &'a GroupId,
    pub(crate) member_pubkey: &'a PublicKey,
    pub(crate) leaf_index: u32,
    pub(crate) server_pubkey: &'a PublicKey,
    pub(crate) relay_hint: Option<&'a RelayUrl>,
    pub(crate) encrypted_token: &'a str,
    pub(crate) platform: Option<PushPlatform>,
    pub(crate) token_fingerprint: Option<&'a str>,
}

impl LocalGroupPushTokenRow {
    fn into_group_push_token(
        self,
        account_pubkey: PublicKey,
    ) -> Result<GroupPushToken, sqlx::Error> {
        let mls_group_id = GroupId::from_slice(&self.mls_group_id);

        let member_pubkey =
            PublicKey::parse(&self.member_pubkey).map_err(|error| sqlx::Error::ColumnDecode {
                index: "member_pubkey".to_string(),
                source: Box::new(error),
            })?;

        let leaf_index =
            u32::try_from(self.leaf_index).map_err(|error| sqlx::Error::ColumnDecode {
                index: "leaf_index".to_string(),
                source: Box::new(error),
            })?;

        let platform = self
            .platform
            .map(|value| {
                value
                    .parse::<PushPlatform>()
                    .map_err(|error| sqlx::Error::ColumnDecode {
                        index: "platform".to_string(),
                        source: Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            error,
                        )),
                    })
            })
            .transpose()?;

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

        Ok(GroupPushToken {
            account_pubkey,
            mls_group_id,
            member_pubkey,
            leaf_index,
            platform,
            token_fingerprint: self.token_fingerprint,
            server_pubkey,
            relay_hint,
            encrypted_token: self.encrypted_token,
            created_at,
            updated_at,
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

impl GroupPushToken {
    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn upsert(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        member_pubkey: &PublicKey,
        leaf_index: u32,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
        encrypted_token: &str,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        Self::upsert_with_metadata(
            account_pubkey,
            GroupPushTokenUpsert {
                mls_group_id,
                member_pubkey,
                leaf_index,
                server_pubkey,
                relay_hint,
                encrypted_token,
                platform: None,
                token_fingerprint: None,
            },
            pool,
        )
        .await
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn upsert_with_metadata(
        account_pubkey: &PublicKey,
        upsert: GroupPushTokenUpsert<'_>,
        pool: &SqlitePool,
    ) -> Result<Self, sqlx::Error> {
        let now = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, LocalGroupPushTokenRow>(
            "INSERT INTO group_push_tokens (
                 mls_group_id,
                 member_pubkey,
                 leaf_index,
                 platform,
                 token_fingerprint,
                 server_pubkey,
                 relay_hint,
                 encrypted_token,
                 created_at,
                 updated_at
             )
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(mls_group_id, leaf_index) DO UPDATE SET
                 member_pubkey = excluded.member_pubkey,
                 platform = excluded.platform,
                 token_fingerprint = excluded.token_fingerprint,
                 server_pubkey = excluded.server_pubkey,
                 relay_hint = excluded.relay_hint,
                 encrypted_token = excluded.encrypted_token,
                 updated_at = excluded.updated_at
             RETURNING mls_group_id, member_pubkey, leaf_index, platform, token_fingerprint, server_pubkey,
                       relay_hint, encrypted_token, created_at, updated_at",
        )
        .bind(upsert.mls_group_id.as_slice())
        .bind(upsert.member_pubkey.to_hex())
        .bind(i64::from(upsert.leaf_index))
        .bind(upsert.platform.map(|platform| platform.as_str().to_string()))
        .bind(upsert.token_fingerprint)
        .bind(upsert.server_pubkey.to_hex())
        .bind(upsert.relay_hint.map(super::utils::normalize_relay_url))
        .bind(upsert.encrypted_token)
        .bind(now)
        .bind(now)
        .fetch_one(pool)
        .await?;

        row.into_group_push_token(*account_pubkey)
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn delete(
        mls_group_id: &GroupId,
        leaf_index: u32,
        pool: &SqlitePool,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM group_push_tokens
             WHERE mls_group_id = ? AND leaf_index = ?",
        )
        .bind(mls_group_id.as_slice())
        .bind(i64::from(leaf_index))
        .execute(pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn delete_by_group(
        mls_group_id: &GroupId,
        pool: &SqlitePool,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM group_push_tokens WHERE mls_group_id = ?")
            .bind(mls_group_id.as_slice())
            .execute(pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn delete_by_member_pubkey(
        mls_group_id: &GroupId,
        member_pubkey: &PublicKey,
        pool: &SqlitePool,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM group_push_tokens
             WHERE mls_group_id = ? AND member_pubkey = ?",
        )
        .bind(mls_group_id.as_slice())
        .bind(member_pubkey.to_hex())
        .execute(pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn find_by_account_and_group(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        pool: &SqlitePool,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query_as::<_, LocalGroupPushTokenRow>(
            "SELECT mls_group_id, member_pubkey, leaf_index, platform, token_fingerprint, server_pubkey,
                    relay_hint, encrypted_token, created_at, updated_at
             FROM group_push_tokens
             WHERE mls_group_id = ?
             ORDER BY leaf_index ASC",
        )
        .bind(mls_group_id.as_slice())
        .fetch_all(pool)
        .await?;

        rows.into_iter()
            .map(|r| r.into_group_push_token(*account_pubkey))
            .collect()
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn group_ids_for_account(
        pool: &SqlitePool,
    ) -> Result<Vec<GroupId>, sqlx::Error> {
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT DISTINCT mls_group_id
             FROM group_push_tokens
             ORDER BY mls_group_id ASC",
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(mls_group_id_bytes,)| GroupId::from_slice(&mls_group_id_bytes))
            .collect())
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn upsert_active_token_list_response(
        mls_group_id: &GroupId,
        active_leaf_map: &BTreeMap<u32, PublicKey>,
        tokens: Vec<LeafTokenTag>,
        pool: &SqlitePool,
    ) -> Result<(), sqlx::Error> {
        let mut tx = pool.begin().await?;
        let now = Utc::now().timestamp_millis();

        for token in tokens {
            let Some(member_pubkey) = active_leaf_map.get(&token.leaf_index) else {
                continue;
            };

            sqlx::query(
                "INSERT INTO group_push_tokens (
                     mls_group_id,
                     member_pubkey,
                     leaf_index,
                     platform,
                     token_fingerprint,
                     server_pubkey,
                     relay_hint,
                     encrypted_token,
                     created_at,
                     updated_at
                 )
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(mls_group_id, leaf_index) DO UPDATE SET
                     member_pubkey = excluded.member_pubkey,
                     platform = excluded.platform,
                     token_fingerprint = excluded.token_fingerprint,
                     server_pubkey = excluded.server_pubkey,
                     relay_hint = excluded.relay_hint,
                     encrypted_token = excluded.encrypted_token,
                     updated_at = excluded.updated_at",
            )
            .bind(mls_group_id.as_slice())
            .bind(member_pubkey.to_hex())
            .bind(i64::from(token.leaf_index))
            .bind(
                token
                    .token_tag
                    .platform
                    .map(|platform| platform.as_str().to_string()),
            )
            .bind(token.token_tag.token_fingerprint.as_deref())
            .bind(token.token_tag.server_pubkey.to_hex())
            .bind(super::utils::normalize_relay_url(
                &token.token_tag.relay_hint,
            ))
            .bind(token.token_tag.encrypted_token.to_base64())
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use nostr_sdk::{Keys, RelayUrl};
    use sqlx::SqlitePool;
    use tempfile::TempDir;

    use super::*;

    fn make_group_id(seed: u8) -> GroupId {
        GroupId::from_slice(&[seed; 32])
    }

    async fn setup() -> (SqlitePool, PublicKey, TempDir) {
        let dir = TempDir::new().unwrap();
        let pool = SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("account.db").display()
        ))
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE group_push_tokens (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id    BLOB NOT NULL,
                member_pubkey   TEXT NOT NULL,
                leaf_index      INTEGER NOT NULL CHECK (leaf_index >= 0),
                platform        TEXT CHECK (platform IN ('apns', 'fcm')),
                token_fingerprint TEXT CHECK (
                    token_fingerprint IS NULL OR
                    token_fingerprint GLOB 'sha256:[0-9a-f]*'
                ),
                server_pubkey   TEXT NOT NULL,
                relay_hint      TEXT,
                encrypted_token TEXT NOT NULL CHECK (
                    length(trim(encrypted_token, ' ' || char(9) || char(10) || char(13))) > 0
                ),
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL,
                UNIQUE(mls_group_id, leaf_index)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let pubkey = Keys::generate().public_key();
        (pool, pubkey, dir)
    }

    #[tokio::test]
    async fn test_group_push_tokens_insert_replace_delete() {
        let (pool, account_pubkey, _dir) = setup().await;
        let mls_group_id = make_group_id(1);
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();
        let member_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://server.example.com/").unwrap();

        let created = GroupPushToken::upsert(
            &account_pubkey,
            &mls_group_id,
            &member_pubkey,
            7,
            &first_server,
            Some(&relay_hint),
            "ciphertext-one",
            &pool,
        )
        .await
        .unwrap();
        assert_eq!(created.leaf_index, 7);
        assert_eq!(created.server_pubkey, first_server);
        assert_eq!(created.encrypted_token, "ciphertext-one");

        let replaced = GroupPushToken::upsert(
            &account_pubkey,
            &mls_group_id,
            &member_pubkey,
            7,
            &second_server,
            None,
            "ciphertext-two",
            &pool,
        )
        .await
        .unwrap();
        assert_eq!(replaced.leaf_index, 7);
        assert_eq!(replaced.server_pubkey, second_server);
        assert_eq!(replaced.relay_hint, None);
        assert_eq!(replaced.encrypted_token, "ciphertext-two");
        assert!(replaced.updated_at >= created.updated_at);

        let stored =
            GroupPushToken::find_by_account_and_group(&account_pubkey, &mls_group_id, &pool)
                .await
                .unwrap();
        assert_eq!(stored, vec![replaced.clone()]);

        let deleted = GroupPushToken::delete(&mls_group_id, 7, &pool)
            .await
            .unwrap();
        assert!(deleted);

        let stored_after_delete =
            GroupPushToken::find_by_account_and_group(&account_pubkey, &mls_group_id, &pool)
                .await
                .unwrap();
        assert!(stored_after_delete.is_empty());
    }

    #[tokio::test]
    async fn test_group_push_tokens_preserve_darkmatter_push_metadata() {
        let (pool, account_pubkey, _dir) = setup().await;
        let mls_group_id = make_group_id(2);
        let member_pubkey = Keys::generate().public_key();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        let created = GroupPushToken::upsert_with_metadata(
            &account_pubkey,
            GroupPushTokenUpsert {
                mls_group_id: &mls_group_id,
                member_pubkey: &member_pubkey,
                leaf_index: 5,
                server_pubkey: &server_pubkey,
                relay_hint: Some(&relay_hint),
                encrypted_token: "ciphertext",
                platform: Some(crate::whitenoise::push_notifications::PushPlatform::Fcm),
                token_fingerprint: Some("sha256:1234567890abcdef12345678"),
            },
            &pool,
        )
        .await
        .unwrap();

        assert_eq!(
            created.platform,
            Some(crate::whitenoise::push_notifications::PushPlatform::Fcm)
        );
        assert_eq!(
            created.token_fingerprint.as_deref(),
            Some("sha256:1234567890abcdef12345678")
        );

        let loaded =
            GroupPushToken::find_by_account_and_group(&account_pubkey, &mls_group_id, &pool)
                .await
                .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].platform,
            Some(crate::whitenoise::push_notifications::PushPlatform::Fcm)
        );
        assert_eq!(
            loaded[0].token_fingerprint.as_deref(),
            Some("sha256:1234567890abcdef12345678")
        );
    }

    #[tokio::test]
    async fn test_group_push_tokens_upsert_allows_multiple_leaves_for_same_member() {
        let (pool, account_pubkey, _dir) = setup().await;
        let mls_group_id = make_group_id(9);
        let member_pubkey = Keys::generate().public_key();
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();

        GroupPushToken::upsert(
            &account_pubkey,
            &mls_group_id,
            &member_pubkey,
            3,
            &first_server,
            None,
            "ciphertext-one",
            &pool,
        )
        .await
        .unwrap();
        let second_leaf = GroupPushToken::upsert(
            &account_pubkey,
            &mls_group_id,
            &member_pubkey,
            7,
            &second_server,
            None,
            "ciphertext-two",
            &pool,
        )
        .await
        .unwrap();

        let stored =
            GroupPushToken::find_by_account_and_group(&account_pubkey, &mls_group_id, &pool)
                .await
                .unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(second_leaf.leaf_index, 7);
        assert_eq!(second_leaf.server_pubkey, second_server);
        assert!(stored.iter().any(|token| token.leaf_index == 3));
        assert!(stored.iter().any(|token| token == &second_leaf));
    }

    #[tokio::test]
    async fn test_group_push_tokens_delete_by_member_pubkey_removes_all_leaves() {
        let (pool, account_pubkey, _dir) = setup().await;
        let mls_group_id = make_group_id(10);
        let member_pubkey = Keys::generate().public_key();
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();

        GroupPushToken::upsert(
            &account_pubkey,
            &mls_group_id,
            &member_pubkey,
            3,
            &first_server,
            None,
            "ciphertext-one",
            &pool,
        )
        .await
        .unwrap();
        GroupPushToken::upsert(
            &account_pubkey,
            &mls_group_id,
            &member_pubkey,
            7,
            &second_server,
            None,
            "ciphertext-two",
            &pool,
        )
        .await
        .unwrap();

        let deleted = GroupPushToken::delete_by_member_pubkey(&mls_group_id, &member_pubkey, &pool)
            .await
            .unwrap();
        assert!(deleted);

        let stored =
            GroupPushToken::find_by_account_and_group(&account_pubkey, &mls_group_id, &pool)
                .await
                .unwrap();
        assert!(stored.is_empty());
    }

    #[tokio::test]
    async fn test_group_push_tokens_scoped_by_group() {
        let (pool, account_pubkey, _dir) = setup().await;
        let group_one = make_group_id(11);
        let group_two = make_group_id(22);
        let first_member = Keys::generate().public_key();
        let second_member = Keys::generate().public_key();
        let server_pubkey = Keys::generate().public_key();

        GroupPushToken::upsert(
            &account_pubkey,
            &group_one,
            &first_member,
            1,
            &server_pubkey,
            None,
            "g1l1",
            &pool,
        )
        .await
        .unwrap();
        GroupPushToken::upsert(
            &account_pubkey,
            &group_two,
            &second_member,
            2,
            &server_pubkey,
            None,
            "g2l2",
            &pool,
        )
        .await
        .unwrap();

        let group_one_tokens =
            GroupPushToken::find_by_account_and_group(&account_pubkey, &group_one, &pool)
                .await
                .unwrap();
        let group_two_tokens =
            GroupPushToken::find_by_account_and_group(&account_pubkey, &group_two, &pool)
                .await
                .unwrap();
        let all_groups = GroupPushToken::group_ids_for_account(&pool).await.unwrap();

        assert_eq!(group_one_tokens.len(), 1);
        assert_eq!(group_one_tokens[0].encrypted_token, "g1l1");
        assert_eq!(group_two_tokens.len(), 1);
        assert_eq!(group_two_tokens[0].encrypted_token, "g2l2");
        assert_eq!(all_groups, vec![group_one, group_two]);
    }

    #[tokio::test]
    async fn test_group_push_tokens_upsert_preserves_created_at() {
        let (pool, account_pubkey, _dir) = setup().await;
        let mls_group_id = make_group_id(33);
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();
        let member_pubkey = Keys::generate().public_key();

        GroupPushToken::upsert(
            &account_pubkey,
            &mls_group_id,
            &member_pubkey,
            4,
            &first_server,
            None,
            "ciphertext-one",
            &pool,
        )
        .await
        .unwrap();

        let created_at_ms = Utc::now().timestamp_millis() - 60_000;
        sqlx::query(
            "UPDATE group_push_tokens
             SET created_at = ?
             WHERE mls_group_id = ? AND leaf_index = ?",
        )
        .bind(created_at_ms)
        .bind(mls_group_id.as_slice())
        .bind(4_i64)
        .execute(&pool)
        .await
        .unwrap();

        let expected_created_at = DateTime::from_timestamp_millis(created_at_ms).unwrap();

        let updated = GroupPushToken::upsert(
            &account_pubkey,
            &mls_group_id,
            &member_pubkey,
            4,
            &second_server,
            None,
            "ciphertext-two",
            &pool,
        )
        .await
        .unwrap();

        assert_eq!(updated.created_at, expected_created_at);
        assert_eq!(updated.server_pubkey, second_server);
        assert_eq!(updated.encrypted_token, "ciphertext-two");
    }

    #[tokio::test]
    async fn test_group_push_tokens_upsert_rejects_whitespace_only_encrypted_token() {
        let (pool, account_pubkey, _dir) = setup().await;
        let mls_group_id = make_group_id(44);
        let server_pubkey = Keys::generate().public_key();
        let member_pubkey = Keys::generate().public_key();

        let error = GroupPushToken::upsert(
            &account_pubkey,
            &mls_group_id,
            &member_pubkey,
            1,
            &server_pubkey,
            None,
            " \n\t\r ",
            &pool,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, sqlx::Error::Database(_)));
    }

    #[test]
    fn test_group_push_token_debug_redacts_encrypted_token() {
        let token = GroupPushToken {
            account_pubkey: Keys::generate().public_key(),
            mls_group_id: make_group_id(55),
            member_pubkey: Keys::generate().public_key(),
            leaf_index: 2,
            platform: None,
            token_fingerprint: None,
            server_pubkey: Keys::generate().public_key(),
            relay_hint: Some(RelayUrl::parse("wss://push.example.com").unwrap()),
            encrypted_token: "ciphertext-value".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let debug_output = format!("{token:?}");

        assert!(debug_output.contains("<redacted>"));
        assert!(!debug_output.contains("ciphertext-value"));
    }
}
