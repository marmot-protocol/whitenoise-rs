//! Per-account NIP-51 mute list rows (pubkeys muted by `account_pubkey`).

use nostr_sdk::PublicKey;

use super::Database;
use crate::perf_instrument;

pub(crate) struct AccountMutedUsers;

impl AccountMutedUsers {
    /// Replaces the full mute set for `account_pubkey` with `muted` (transactional).
    #[perf_instrument("db::account_muted_users")]
    pub(crate) async fn replace_all_for_account(
        account_pubkey: &PublicKey,
        muted: &[PublicKey],
        database: &Database,
    ) -> std::result::Result<(), sqlx::Error> {
        let mut txn = database.pool.begin().await?;
        let account_hex = account_pubkey.to_hex();
        let now = chrono::Utc::now().timestamp_millis();

        sqlx::query("DELETE FROM account_muted_users WHERE account_pubkey = ?")
            .bind(&account_hex)
            .execute(&mut *txn)
            .await?;

        for pk in muted {
            sqlx::query(
                "INSERT INTO account_muted_users (account_pubkey, muted_pubkey, created_at, updated_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(&account_hex)
            .bind(pk.to_hex())
            .bind(now)
            .bind(now)
            .execute(&mut *txn)
            .await?;
        }

        txn.commit().await?;
        Ok(())
    }

    /// Returns true if `target` is in `account_pubkey`'s stored mute list.
    #[perf_instrument("db::account_muted_users")]
    pub(crate) async fn is_muted(
        account_pubkey: &PublicKey,
        target: &PublicKey,
        database: &Database,
    ) -> std::result::Result<bool, sqlx::Error> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM account_muted_users
             WHERE account_pubkey = ? AND muted_pubkey = ?",
        )
        .bind(account_pubkey.to_hex())
        .bind(target.to_hex())
        .fetch_optional(&database.pool)
        .await?;

        Ok(row.is_some())
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;

    use super::*;
    use crate::whitenoise::test_utils::*;

    async fn insert_test_account(database: &Database, pubkey: &PublicKey) {
        let user_pubkey = pubkey.to_hex();
        sqlx::query("INSERT INTO users (pubkey, metadata) VALUES (?, '{}')")
            .bind(&user_pubkey)
            .execute(&database.pool)
            .await
            .expect("insert user");
        let (user_id,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE pubkey = ?")
            .bind(&user_pubkey)
            .fetch_one(&database.pool)
            .await
            .expect("get user id");
        sqlx::query("INSERT INTO accounts (pubkey, user_id, last_synced_at) VALUES (?, ?, NULL)")
            .bind(&user_pubkey)
            .bind(user_id)
            .execute(&database.pool)
            .await
            .expect("insert account");
    }

    #[tokio::test]
    async fn test_replace_all_and_is_muted() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let account = Keys::generate();
        let a = Keys::generate();
        let b = Keys::generate();
        insert_test_account(&whitenoise.database, &account.public_key()).await;

        AccountMutedUsers::replace_all_for_account(
            &account.public_key(),
            &[a.public_key(), b.public_key()],
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(
            AccountMutedUsers::is_muted(
                &account.public_key(),
                &a.public_key(),
                &whitenoise.database
            )
            .await
            .unwrap()
        );
        assert!(
            AccountMutedUsers::is_muted(
                &account.public_key(),
                &b.public_key(),
                &whitenoise.database
            )
            .await
            .unwrap()
        );
        assert!(
            !AccountMutedUsers::is_muted(
                &account.public_key(),
                &Keys::generate().public_key(),
                &whitenoise.database
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn test_replace_all_clears_previous() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let account = Keys::generate();
        let a = Keys::generate();
        let b = Keys::generate();
        insert_test_account(&whitenoise.database, &account.public_key()).await;

        AccountMutedUsers::replace_all_for_account(
            &account.public_key(),
            &[a.public_key()],
            &whitenoise.database,
        )
        .await
        .unwrap();
        AccountMutedUsers::replace_all_for_account(
            &account.public_key(),
            &[b.public_key()],
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(
            !AccountMutedUsers::is_muted(
                &account.public_key(),
                &a.public_key(),
                &whitenoise.database
            )
            .await
            .unwrap()
        );
        assert!(
            AccountMutedUsers::is_muted(
                &account.public_key(),
                &b.public_key(),
                &whitenoise.database
            )
            .await
            .unwrap()
        );
    }
}
