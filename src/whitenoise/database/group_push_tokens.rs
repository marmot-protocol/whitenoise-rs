//! Database operations for cached per-group push tokens.

use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::{PublicKey, RelayUrl};

use super::{Database, utils::parse_timestamp};
use crate::perf_span;
use crate::whitenoise::push_notifications::GroupPushToken;

#[derive(Debug)]
struct GroupPushTokenRow {
    account_pubkey: PublicKey,
    group_id: GroupId,
    leaf_index: u32,
    server_pubkey: PublicKey,
    relay_hint: Option<RelayUrl>,
    encrypted_token: String,
    updated_at: DateTime<Utc>,
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

        let group_id_bytes: Vec<u8> = row.try_get("group_id")?;
        let group_id = GroupId::from_slice(&group_id_bytes);

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
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(Self {
            account_pubkey,
            group_id,
            leaf_index,
            server_pubkey,
            relay_hint,
            encrypted_token,
            updated_at,
        })
    }
}

impl From<GroupPushTokenRow> for GroupPushToken {
    fn from(row: GroupPushTokenRow) -> Self {
        Self {
            account_pubkey: row.account_pubkey,
            group_id: row.group_id,
            leaf_index: row.leaf_index,
            server_pubkey: row.server_pubkey,
            relay_hint: row.relay_hint,
            encrypted_token: row.encrypted_token,
            updated_at: row.updated_at,
        }
    }
}

// PR 1 only persists the cache locally; gossip/publication flows consume these
// helpers in follow-up PRs, while tests exercise them now.
#[allow(dead_code)]
impl GroupPushToken {
    pub(crate) async fn upsert(
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        leaf_index: u32,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
        encrypted_token: &str,
        database: &Database,
    ) -> Result<Self, sqlx::Error> {
        let _span = perf_span!("db::group_push_tokens_upsert");
        let now = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, GroupPushTokenRow>(
            "INSERT INTO group_push_tokens (
                 account_pubkey,
                 group_id,
                 leaf_index,
                 server_pubkey,
                 relay_hint,
                 encrypted_token,
                 updated_at
             )
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_pubkey, group_id, leaf_index) DO UPDATE SET
                 server_pubkey = excluded.server_pubkey,
                 relay_hint = excluded.relay_hint,
                 encrypted_token = excluded.encrypted_token,
                 updated_at = excluded.updated_at
             RETURNING *",
        )
        .bind(account_pubkey.to_hex())
        .bind(group_id.as_slice())
        .bind(i64::from(leaf_index))
        .bind(server_pubkey.to_hex())
        .bind(relay_hint.map(super::utils::normalize_relay_url))
        .bind(encrypted_token)
        .bind(now)
        .fetch_one(&database.pool)
        .await?;

        Ok(row.into())
    }

    pub(crate) async fn delete(
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        leaf_index: u32,
        database: &Database,
    ) -> Result<bool, sqlx::Error> {
        let _span = perf_span!("db::group_push_tokens_delete");
        let result = sqlx::query(
            "DELETE FROM group_push_tokens
             WHERE account_pubkey = ? AND group_id = ? AND leaf_index = ?",
        )
        .bind(account_pubkey.to_hex())
        .bind(group_id.as_slice())
        .bind(i64::from(leaf_index))
        .execute(&database.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub(crate) async fn find_by_account_and_group(
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        database: &Database,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let _span = perf_span!("db::group_push_tokens_find_by_group");
        let rows = sqlx::query_as::<_, GroupPushTokenRow>(
            "SELECT *
             FROM group_push_tokens
             WHERE account_pubkey = ? AND group_id = ?
             ORDER BY leaf_index ASC",
        )
        .bind(account_pubkey.to_hex())
        .bind(group_id.as_slice())
        .fetch_all(&database.pool)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub(crate) async fn group_ids_for_account(
        account_pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Vec<GroupId>, sqlx::Error> {
        let _span = perf_span!("db::group_push_tokens_group_ids_for_account");
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT DISTINCT group_id
             FROM group_push_tokens
             WHERE account_pubkey = ?
             ORDER BY group_id ASC",
        )
        .bind(account_pubkey.to_hex())
        .fetch_all(&database.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(group_id_bytes,)| GroupId::from_slice(&group_id_bytes))
            .collect())
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
        let group_id = make_group_id(1);
        let first_server = Keys::generate().public_key();
        let second_server = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://server.example.com/").unwrap();

        let created = GroupPushToken::upsert(
            &account.pubkey,
            &group_id,
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
            &group_id,
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
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(stored, vec![replaced.clone()]);

        let deleted = GroupPushToken::delete(&account.pubkey, &group_id, 7, &whitenoise.database)
            .await
            .unwrap();
        assert!(deleted);

        let stored_after_delete = GroupPushToken::find_by_account_and_group(
            &account.pubkey,
            &group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(stored_after_delete.is_empty());
    }

    #[tokio::test]
    async fn test_group_push_tokens_are_scoped_by_account_and_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account_one = whitenoise.create_identity().await.unwrap();
        let account_two = whitenoise.create_identity().await.unwrap();
        let group_one = make_group_id(11);
        let group_two = make_group_id(22);
        let server_pubkey = Keys::generate().public_key();

        GroupPushToken::upsert(
            &account_one.pubkey,
            &group_one,
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
}
