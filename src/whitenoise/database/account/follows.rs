//! Per-account repository for follow relationships.
//!
//! Wraps the existing [`Account`] follow DB methods so that callers do not
//! need to thread an `account_pubkey` argument through every call — the pubkey
//! is baked in at construction time.
//!
//! Because the underlying `account_follows` table is keyed by integer
//! `account_id`, construction eagerly resolves the integer primary key once
//! and stores it directly. All subsequent operations use that stored `i64`
//! without further DB round-trips to load the full [`Account`] row.

use std::sync::Arc;

use nostr_sdk::PublicKey;

use crate::whitenoise::database::{Database, DatabaseError};
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::users::User;

/// Repository for follow relationships scoped to a single account.
#[derive(Clone, Debug)]
pub struct AccountFollowsRepo {
    /// Immutable integer primary key for the account row.
    ///
    /// Resolved once at construction; the `accounts.id` column is a stable FK
    /// that never changes after insertion.
    account_id: i64,
    db: Arc<Database>,
}

impl AccountFollowsRepo {
    /// Construct a new [`AccountFollowsRepo`] for `account_pubkey`.
    ///
    /// Performs a single DB lookup to resolve the integer account id. Returns
    /// an error if no account row exists for `account_pubkey`.
    pub(crate) async fn new(account_pubkey: PublicKey, db: Arc<Database>) -> Result<Self> {
        let account_id = Self::resolve_account_id(&account_pubkey, &db).await?;
        Ok(Self { account_id, db })
    }

    async fn resolve_account_id(pubkey: &PublicKey, db: &Database) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM accounts WHERE pubkey = ?")
            .bind(pubkey.to_hex())
            .fetch_optional(&db.pool)
            .await
            .map_err(DatabaseError::Sqlx)?;
        row.map(|(id,)| id).ok_or(WhitenoiseError::AccountNotFound)
    }

    /// Return all users followed by this account.
    ///
    /// The SQL here mirrors `Account::follows` in `database/accounts.rs`. Both
    /// coexist during the session/projection migration; `Account::follows` will
    /// be removed once all callers have moved to this repo.
    pub async fn all(&self) -> Result<Vec<User>> {
        let users = sqlx::query_as::<_, User>(
            "SELECT u.id, u.pubkey, u.metadata, u.created_at, u.metadata_known_at, u.updated_at
             FROM account_follows af
             JOIN users u ON af.user_id = u.id
             WHERE af.account_id = ?",
        )
        .bind(self.account_id)
        .fetch_all(&self.db.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;
        Ok(users)
    }

    /// Return whether this account follows `target_pubkey`.
    ///
    /// Returns `false` (rather than an error) when `target_pubkey` has no user
    /// record in the database, matching the existing behaviour in
    /// `Whitenoise::is_following_user`.
    pub async fn is_following(&self, target_pubkey: &PublicKey) -> Result<bool> {
        let row: Option<(bool,)> = sqlx::query_as(
            "SELECT EXISTS (
                 SELECT 1 FROM account_follows af
                 JOIN users u ON af.user_id = u.id
                 WHERE u.pubkey = ? AND af.account_id = ?
             )",
        )
        .bind(target_pubkey.to_hex())
        .bind(self.account_id)
        .fetch_optional(&self.db.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;
        Ok(row.map(|(exists,)| exists).unwrap_or(false))
    }

    /// Record that this account follows `user`.
    pub async fn add(&self, user: &User) -> Result<()> {
        let user_id = user
            .id
            .ok_or_else(|| DatabaseError::MissingUserId(user.pubkey.to_hex()))?;
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO account_follows (account_id, user_id, created_at, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(account_id, user_id) DO UPDATE SET updated_at = ?",
        )
        .bind(self.account_id)
        .bind(user_id)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&self.db.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;
        Ok(())
    }

    /// Remove the follow relationship between this account and `user`.
    pub async fn remove(&self, user: &User) -> Result<()> {
        let user_id = user
            .id
            .ok_or_else(|| DatabaseError::MissingUserId(user.pubkey.to_hex()))?;
        sqlx::query("DELETE FROM account_follows WHERE account_id = ? AND user_id = ?")
            .bind(self.account_id)
            .bind(user_id)
            .execute(&self.db.pool)
            .await
            .map_err(DatabaseError::Sqlx)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::{Keys, PublicKey};

    use super::AccountFollowsRepo;
    use crate::whitenoise::database::Database;
    use crate::whitenoise::test_utils::{create_mock_whitenoise, insert_test_account};
    use crate::whitenoise::users::User;

    async fn insert_test_user(database: &Database, pubkey: &PublicKey) -> User {
        let (user, _) = User::find_or_create_by_pubkey(pubkey, database)
            .await
            .expect("create user");
        user
    }

    #[tokio::test]
    async fn all_returns_empty_for_new_account() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone())
            .await
            .unwrap();
        assert!(repo.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_and_all_returns_followed_user() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let target_pk = Keys::generate().public_key();
        let target_user = insert_test_user(&wn.database, &target_pk).await;

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone())
            .await
            .unwrap();
        repo.add(&target_user).await.unwrap();

        let follows = repo.all().await.unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0].pubkey, target_pk);
    }

    #[tokio::test]
    async fn is_following_returns_false_when_not_following() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let target_pk = Keys::generate().public_key();
        insert_test_user(&wn.database, &target_pk).await;

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone())
            .await
            .unwrap();
        assert!(!repo.is_following(&target_pk).await.unwrap());
    }

    #[tokio::test]
    async fn is_following_returns_false_for_unknown_user() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let unknown_pk = Keys::generate().public_key();

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone())
            .await
            .unwrap();
        assert!(!repo.is_following(&unknown_pk).await.unwrap());
    }

    #[tokio::test]
    async fn add_then_is_following_returns_true() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let target_pk = Keys::generate().public_key();
        let target_user = insert_test_user(&wn.database, &target_pk).await;

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone())
            .await
            .unwrap();
        repo.add(&target_user).await.unwrap();

        assert!(repo.is_following(&target_pk).await.unwrap());
    }

    #[tokio::test]
    async fn remove_clears_follow() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let target_pk = Keys::generate().public_key();
        let target_user = insert_test_user(&wn.database, &target_pk).await;

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone())
            .await
            .unwrap();
        repo.add(&target_user).await.unwrap();
        repo.remove(&target_user).await.unwrap();

        assert!(!repo.is_following(&target_pk).await.unwrap());
        assert!(repo.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn remove_nonexistent_is_noop() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let target_pk = Keys::generate().public_key();
        let target_user = insert_test_user(&wn.database, &target_pk).await;

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone())
            .await
            .unwrap();
        assert!(repo.remove(&target_user).await.is_ok());
    }

    #[tokio::test]
    async fn repos_for_different_accounts_are_isolated() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys_a = Keys::generate();
        let keys_b = Keys::generate();
        insert_test_account(&wn.database, &keys_a.public_key()).await;
        insert_test_account(&wn.database, &keys_b.public_key()).await;
        let target_pk = Keys::generate().public_key();
        let target_user = insert_test_user(&wn.database, &target_pk).await;

        let repo_a = AccountFollowsRepo::new(keys_a.public_key(), wn.database.clone())
            .await
            .unwrap();
        let repo_b = AccountFollowsRepo::new(keys_b.public_key(), wn.database.clone())
            .await
            .unwrap();

        repo_a.add(&target_user).await.unwrap();

        assert!(repo_a.is_following(&target_pk).await.unwrap());
        assert!(!repo_b.is_following(&target_pk).await.unwrap());
    }

    #[tokio::test]
    async fn new_returns_error_for_unknown_account() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let unknown_pk = Keys::generate().public_key();

        let result = AccountFollowsRepo::new(unknown_pk, wn.database.clone()).await;
        assert!(result.is_err());
    }
}
