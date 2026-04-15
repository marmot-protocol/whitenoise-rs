//! Per-account repository for follow relationships.
//!
//! Wraps the existing [`Account`] follow DB methods so that callers do not
//! need to thread an `account_pubkey` argument through every call — the pubkey
//! is baked in at construction time.
//!
//! Because the underlying `account_follows` table is keyed by integer
//! `account_id`, each operation first loads the [`Account`] row to resolve the
//! primary key, then delegates to the existing DB methods.

use std::sync::Arc;

use nostr_sdk::PublicKey;

use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::Database;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::users::User;

/// Repository for follow relationships scoped to a single account.
#[derive(Clone, Debug)]
pub struct AccountFollowsRepo {
    account_pubkey: PublicKey,
    db: Arc<Database>,
}

impl AccountFollowsRepo {
    /// Construct a new [`AccountFollowsRepo`] for `account_pubkey`.
    pub(crate) fn new(account_pubkey: PublicKey, db: Arc<Database>) -> Self {
        Self { account_pubkey, db }
    }

    /// Load the [`Account`] row for this repo's pubkey.
    async fn load_account(&self) -> Result<Account> {
        Account::find_by_pubkey(&self.account_pubkey, &self.db).await
    }

    /// Return all users followed by this account.
    ///
    /// Delegates to `Account::follows` with the baked-in account pubkey.
    pub async fn all(&self) -> Result<Vec<User>> {
        let account = self.load_account().await?;
        account.follows(&self.db).await
    }

    /// Return whether this account follows `target_pubkey`.
    ///
    /// Returns `false` (rather than an error) when `target_pubkey` has no user
    /// record in the database, matching the existing behaviour in
    /// `Whitenoise::is_following_user`.
    ///
    /// Delegates to `Account::is_following_user` with the baked-in account
    /// pubkey.
    pub async fn is_following(&self, target_pubkey: &PublicKey) -> Result<bool> {
        let user = match User::find_by_pubkey(target_pubkey, &self.db).await {
            Ok(user) => user,
            Err(WhitenoiseError::UserNotFound) => return Ok(false),
            Err(e) => return Err(e),
        };
        let account = self.load_account().await?;
        account.is_following_user(&user, &self.db).await
    }

    /// Record that this account follows `user`.
    ///
    /// Delegates to `Account::follow_user` with the baked-in account pubkey.
    pub async fn add(&self, user: &User) -> Result<()> {
        let account = self.load_account().await?;
        account.follow_user(user, &self.db).await
    }

    /// Remove the follow relationship between this account and `user`.
    ///
    /// Delegates to `Account::unfollow_user` with the baked-in account
    /// pubkey.
    pub async fn remove(&self, user: &User) -> Result<()> {
        let account = self.load_account().await?;
        account.unfollow_user(user, &self.db).await
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::{Keys, PublicKey};

    use super::AccountFollowsRepo;
    use crate::whitenoise::database::Database;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use crate::whitenoise::users::User;

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

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone());
        assert!(repo.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_and_all_returns_followed_user() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let target_pk = Keys::generate().public_key();
        let target_user = insert_test_user(&wn.database, &target_pk).await;

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone());
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

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone());
        assert!(!repo.is_following(&target_pk).await.unwrap());
    }

    #[tokio::test]
    async fn is_following_returns_false_for_unknown_user() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let unknown_pk = Keys::generate().public_key();

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone());
        assert!(!repo.is_following(&unknown_pk).await.unwrap());
    }

    #[tokio::test]
    async fn add_then_is_following_returns_true() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let target_pk = Keys::generate().public_key();
        let target_user = insert_test_user(&wn.database, &target_pk).await;

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone());
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

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone());
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

        let repo = AccountFollowsRepo::new(keys.public_key(), wn.database.clone());
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

        let repo_a = AccountFollowsRepo::new(keys_a.public_key(), wn.database.clone());
        let repo_b = AccountFollowsRepo::new(keys_b.public_key(), wn.database.clone());

        repo_a.add(&target_user).await.unwrap();

        assert!(repo_a.is_following(&target_pk).await.unwrap());
        assert!(!repo_b.is_following(&target_pk).await.unwrap());
    }
}
