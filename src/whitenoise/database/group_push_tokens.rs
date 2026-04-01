//! Database operations for cached per-group push tokens.

use core::fmt;
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::{PublicKey, RelayUrl};

use super::{Database, utils::parse_timestamp};
use crate::perf_instrument;
use crate::whitenoise::push_notifications::GroupPushToken;

struct GroupPushTokenRow {
    account_pubkey: PublicKey,
    mls_group_id: GroupId,
    member_pubkey: PublicKey,
    leaf_index: u32,
    server_pubkey: PublicKey,
    relay_hint: Option<RelayUrl>,
    encrypted_token: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl fmt::Debug for GroupPushTokenRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupPushTokenRow")
            .field("account_pubkey", &self.account_pubkey)
            .field("mls_group_id", &self.mls_group_id)
            .field("member_pubkey", &self.member_pubkey)
            .field("leaf_index", &self.leaf_index)
            .field("server_pubkey", &self.server_pubkey)
            .field("relay_hint", &self.relay_hint)
            .field("encrypted_token", &"<redacted>")
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl<'r, R> sqlx::FromRow<'r, R> for GroupPushTokenRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<String>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> Result<Self, sqlx::Error> {
        let account_pubkey_str: String = row.try_get("account_pubkey")?;
        let account_pubkey =
            PublicKey::parse(&account_pubkey_str).map_err(|error| sqlx::Error::ColumnDecode {
                index: "account_pubkey".to_string(),
                source: Box::new(error),
            })?;

        let mls_group_id_bytes: Vec<u8> = row.try_get("mls_group_id")?;
        let mls_group_id = GroupId::from_slice(&mls_group_id_bytes);

        let member_pubkey_str: String = row.try_get("member_pubkey")?;
        let member_pubkey =
            PublicKey::parse(&member_pubkey_str).map_err(|error| sqlx::Error::ColumnDecode {
                index: "member_pubkey".to_string(),
                source: Box::new(error),
            })?;

        let leaf_index_int: i64 = row.try_get("leaf_index")?;
        let leaf_index =
            u32::try_from(leaf_index_int).map_err(|error| sqlx::Error::ColumnDecode {
                index: "leaf_index".to_string(),
                source: Box::new(error),
            })?;

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

        let encrypted_token: String = row.try_get("encrypted_token")?;
        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(Self {
            account_pubkey,
            mls_group_id,
            member_pubkey,
            leaf_index,
            server_pubkey,
            relay_hint,
            encrypted_token,
            created_at,
            updated_at,
        })
    }
}

impl From<GroupPushTokenRow> for GroupPushToken {
    fn from(row: GroupPushTokenRow) -> Self {
        Self {
            account_pubkey: row.account_pubkey,
            mls_group_id: row.mls_group_id,
            member_pubkey: row.member_pubkey,
            leaf_index: row.leaf_index,
            server_pubkey: row.server_pubkey,
            relay_hint: row.relay_hint,
            encrypted_token: row.encrypted_token,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

// PR 1 only persists the cache locally; gossip/publication flows consume these
// helpers in follow-up PRs, while tests exercise them now.
#[allow(dead_code)]
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
        database: &Database,
    ) -> Result<Self, sqlx::Error> {
        let now = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, GroupPushTokenRow>(
            "INSERT INTO group_push_tokens (
                 account_pubkey,
                 mls_group_id,
                 member_pubkey,
                 leaf_index,
                 server_pubkey,
                 relay_hint,
                 encrypted_token,
                 created_at,
                 updated_at
             )
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_pubkey, mls_group_id, leaf_index) DO UPDATE SET
                 member_pubkey = excluded.member_pubkey,
                 server_pubkey = excluded.server_pubkey,
                 relay_hint = excluded.relay_hint,
                 encrypted_token = excluded.encrypted_token,
                 updated_at = excluded.updated_at
             RETURNING *",
        )
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .bind(member_pubkey.to_hex())
        .bind(i64::from(leaf_index))
        .bind(server_pubkey.to_hex())
        .bind(relay_hint.map(super::utils::normalize_relay_url))
        .bind(encrypted_token)
        .bind(now)
        .bind(now)
        .fetch_one(&database.pool)
        .await?;

        Ok(row.into())
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn delete(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        leaf_index: u32,
        database: &Database,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM group_push_tokens
             WHERE account_pubkey = ? AND mls_group_id = ? AND leaf_index = ?",
        )
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .bind(i64::from(leaf_index))
        .execute(&database.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn delete_by_member_pubkey(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        member_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM group_push_tokens
             WHERE account_pubkey = ? AND mls_group_id = ? AND member_pubkey = ?",
        )
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .bind(member_pubkey.to_hex())
        .execute(&database.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn find_by_account_and_group(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        database: &Database,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query_as::<_, GroupPushTokenRow>(
            "SELECT *
             FROM group_push_tokens
             WHERE account_pubkey = ? AND mls_group_id = ?
             ORDER BY leaf_index ASC",
        )
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .fetch_all(&database.pool)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn group_ids_for_account(
        account_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Vec<GroupId>, sqlx::Error> {
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT DISTINCT mls_group_id
             FROM group_push_tokens
             WHERE account_pubkey = ?
             ORDER BY mls_group_id ASC",
        )
        .bind(account_pubkey.to_hex())
        .fetch_all(&database.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(mls_group_id_bytes,)| GroupId::from_slice(&mls_group_id_bytes))
            .collect())
    }

    #[perf_instrument("db::group_push_tokens")]
    pub(crate) async fn upsert_active_token_list_response(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        active_leaf_map: &BTreeMap<u32, PublicKey>,
        tokens: Vec<mdk_core::mip05::LeafTokenTag>,
        database: &Database,
    ) -> Result<(), sqlx::Error> {
        let mut tx = database.pool.begin().await?;
        let now = Utc::now().timestamp_millis();

        for token in tokens {
            let Some(member_pubkey) = active_leaf_map.get(&token.leaf_index) else {
                continue;
            };

            sqlx::query(
                "INSERT INTO group_push_tokens (
                     account_pubkey,
                     mls_group_id,
                     member_pubkey,
                     leaf_index,
                     server_pubkey,
                     relay_hint,
                     encrypted_token,
                     created_at,
                     updated_at
                 )
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(account_pubkey, mls_group_id, leaf_index) DO UPDATE SET
                     member_pubkey = excluded.member_pubkey,
                     server_pubkey = excluded.server_pubkey,
                     relay_hint = excluded.relay_hint,
                     encrypted_token = excluded.encrypted_token,
                     updated_at = excluded.updated_at",
            )
            .bind(account_pubkey.to_hex())
            .bind(mls_group_id.as_slice())
            .bind(member_pubkey.to_hex())
            .bind(i64::from(token.leaf_index))
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
    use nostr_sdk::{Keys, RelayUrl};

    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    fn make_group_id(seed: u8) -> GroupId {
        GroupId::from_slice(&[seed; 32])
    }

    #[tokio::test]
    async fn test_group_push_tokens_insert_replace_delete() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let mls_group_id = make_group_id(1);
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();
        let member_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://server.example.com/").unwrap();

        let created = GroupPushToken::upsert(
            &account.pubkey,
            &mls_group_id,
            &member_pubkey,
            7,
            &first_server,
            Some(&relay_hint),
            "ciphertext-one",
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(created.leaf_index, 7);
        assert_eq!(created.server_pubkey, first_server);
        assert_eq!(created.encrypted_token, "ciphertext-one");

        let replaced = GroupPushToken::upsert(
            &account.pubkey,
            &mls_group_id,
            &member_pubkey,
            7,
            &second_server,
            None,
            "ciphertext-two",
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(replaced.leaf_index, 7);
        assert_eq!(replaced.server_pubkey, second_server);
        assert_eq!(replaced.relay_hint, None);
        assert_eq!(replaced.encrypted_token, "ciphertext-two");
        assert!(replaced.updated_at >= created.updated_at);

        let stored = GroupPushToken::find_by_account_and_group(
            &account.pubkey,
            &mls_group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(stored, vec![replaced.clone()]);

        let deleted =
            GroupPushToken::delete(&account.pubkey, &mls_group_id, 7, &whitenoise.database)
                .await
                .unwrap();
        assert!(deleted);

        let stored_after_delete = GroupPushToken::find_by_account_and_group(
            &account.pubkey,
            &mls_group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(stored_after_delete.is_empty());
    }

    #[tokio::test]
    async fn test_group_push_tokens_upsert_allows_multiple_leaves_for_same_member() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let mls_group_id = make_group_id(9);
        let member_pubkey = Keys::generate().public_key();
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();

        GroupPushToken::upsert(
            &account.pubkey,
            &mls_group_id,
            &member_pubkey,
            3,
            &first_server,
            None,
            "ciphertext-one",
            &whitenoise.database,
        )
        .await
        .unwrap();
        let second_leaf = GroupPushToken::upsert(
            &account.pubkey,
            &mls_group_id,
            &member_pubkey,
            7,
            &second_server,
            None,
            "ciphertext-two",
            &whitenoise.database,
        )
        .await
        .unwrap();

        let stored = GroupPushToken::find_by_account_and_group(
            &account.pubkey,
            &mls_group_id,
            &whitenoise.database,
        )
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
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let mls_group_id = make_group_id(10);
        let member_pubkey = Keys::generate().public_key();
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();

        GroupPushToken::upsert(
            &account.pubkey,
            &mls_group_id,
            &member_pubkey,
            3,
            &first_server,
            None,
            "ciphertext-one",
            &whitenoise.database,
        )
        .await
        .unwrap();
        GroupPushToken::upsert(
            &account.pubkey,
            &mls_group_id,
            &member_pubkey,
            7,
            &second_server,
            None,
            "ciphertext-two",
            &whitenoise.database,
        )
        .await
        .unwrap();

        let deleted = GroupPushToken::delete_by_member_pubkey(
            &account.pubkey,
            &mls_group_id,
            &member_pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(deleted);

        let stored = GroupPushToken::find_by_account_and_group(
            &account.pubkey,
            &mls_group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(stored.is_empty());
    }

    #[tokio::test]
    async fn test_group_push_tokens_are_scoped_by_account_and_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account_one = whitenoise.create_identity().await.unwrap();
        let account_two = whitenoise.create_identity().await.unwrap();
        let group_one = make_group_id(11);
        let group_two = make_group_id(22);
        let first_member = Keys::generate().public_key();
        let second_member = Keys::generate().public_key();
        let third_member = Keys::generate().public_key();
        let server_pubkey = Keys::generate().public_key();

        GroupPushToken::upsert(
            &account_one.pubkey,
            &group_one,
            &first_member,
            1,
            &server_pubkey,
            None,
            "a1g1l1",
            &whitenoise.database,
        )
        .await
        .unwrap();
        GroupPushToken::upsert(
            &account_one.pubkey,
            &group_two,
            &second_member,
            2,
            &server_pubkey,
            None,
            "a1g2l2",
            &whitenoise.database,
        )
        .await
        .unwrap();
        GroupPushToken::upsert(
            &account_two.pubkey,
            &group_one,
            &third_member,
            3,
            &server_pubkey,
            None,
            "a2g1l3",
            &whitenoise.database,
        )
        .await
        .unwrap();

        let account_one_group_one = GroupPushToken::find_by_account_and_group(
            &account_one.pubkey,
            &group_one,
            &whitenoise.database,
        )
        .await
        .unwrap();
        let account_one_group_two = GroupPushToken::find_by_account_and_group(
            &account_one.pubkey,
            &group_two,
            &whitenoise.database,
        )
        .await
        .unwrap();
        let account_two_group_one = GroupPushToken::find_by_account_and_group(
            &account_two.pubkey,
            &group_one,
            &whitenoise.database,
        )
        .await
        .unwrap();
        let account_one_groups =
            GroupPushToken::group_ids_for_account(&account_one.pubkey, &whitenoise.database)
                .await
                .unwrap();

        assert_eq!(account_one_group_one.len(), 1);
        assert_eq!(account_one_group_one[0].encrypted_token, "a1g1l1");
        assert_eq!(account_one_group_two.len(), 1);
        assert_eq!(account_one_group_two[0].encrypted_token, "a1g2l2");
        assert_eq!(account_two_group_one.len(), 1);
        assert_eq!(account_two_group_one[0].encrypted_token, "a2g1l3");
        assert_eq!(account_one_groups, vec![group_one, group_two]);
    }

    #[tokio::test]
    async fn test_group_push_tokens_upsert_preserves_created_at() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let mls_group_id = make_group_id(33);
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();
        let member_pubkey = Keys::generate().public_key();

        GroupPushToken::upsert(
            &account.pubkey,
            &mls_group_id,
            &member_pubkey,
            4,
            &first_server,
            None,
            "ciphertext-one",
            &whitenoise.database,
        )
        .await
        .unwrap();

        let created_at_ms = Utc::now().timestamp_millis() - 60_000;
        sqlx::query(
            "UPDATE group_push_tokens
             SET created_at = ?
             WHERE account_pubkey = ? AND mls_group_id = ? AND leaf_index = ?",
        )
        .bind(created_at_ms)
        .bind(account.pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .bind(4_i64)
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        let expected_created_at = DateTime::from_timestamp_millis(created_at_ms).unwrap();

        let updated = GroupPushToken::upsert(
            &account.pubkey,
            &mls_group_id,
            &member_pubkey,
            4,
            &second_server,
            None,
            "ciphertext-two",
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert_eq!(updated.created_at, expected_created_at);
        assert_eq!(updated.server_pubkey, second_server);
        assert_eq!(updated.encrypted_token, "ciphertext-two");
    }

    #[tokio::test]
    async fn test_group_push_tokens_upsert_rejects_whitespace_only_encrypted_token() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let mls_group_id = make_group_id(44);
        let server_pubkey = Keys::generate().public_key();

        let member_pubkey = Keys::generate().public_key();

        let error = GroupPushToken::upsert(
            &account.pubkey,
            &mls_group_id,
            &member_pubkey,
            1,
            &server_pubkey,
            None,
            " \n\t\r ",
            &whitenoise.database,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, sqlx::Error::Database(_)));
    }

    #[test]
    fn test_group_push_token_row_debug_redacts_encrypted_token() {
        let row = GroupPushTokenRow {
            account_pubkey: Keys::generate().public_key(),
            mls_group_id: make_group_id(55),
            member_pubkey: Keys::generate().public_key(),
            leaf_index: 2,
            server_pubkey: Keys::generate().public_key(),
            relay_hint: Some(RelayUrl::parse("wss://push.example.com").unwrap()),
            encrypted_token: "ciphertext-value".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let debug_output = format!("{row:?}");

        assert!(debug_output.contains("<redacted>"));
        assert!(!debug_output.contains("ciphertext-value"));
    }
}
