//! Per-account repository for follow relationships.
//!
//! Backed by the per-account `account_follows` table (pubkey-keyed; no FK to
//! shared `users`). When a richer view is needed, the repo resolves followed
//! pubkeys to [`User`] rows in the shared DB. Follow lists are typically
//! small (rarely > 1000), so the resolve-then-batch path is fine.

use std::sync::Arc;

use nostr_sdk::PublicKey;

use crate::whitenoise::database::account_db::AccountDatabase;
use crate::whitenoise::database::{Database, DatabaseError};
use crate::whitenoise::error::Result;
use crate::whitenoise::users::User;

/// Repository for follow relationships scoped to a single account.
///
/// Holds an [`AccountDatabase`] for follow-list reads/writes and an
/// [`Arc<Database>`] for resolving followed pubkeys to [`User`] rows on demand.
#[derive(Clone, Debug)]
pub struct AccountFollowsRepo {
    account_db: Arc<AccountDatabase>,
    shared_db: Arc<Database>,
}

impl AccountFollowsRepo {
    /// Construct a new [`AccountFollowsRepo`] backed by the given per-account
    /// DB and the shared DB used for user resolution.
    pub(crate) fn new(account_db: Arc<AccountDatabase>, shared_db: Arc<Database>) -> Self {
        Self {
            account_db,
            shared_db,
        }
    }

    /// Return the followed pubkeys for this account, in pubkey order.
    pub async fn follow_pubkeys(&self) -> Result<Vec<PublicKey>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT pubkey FROM account_follows ORDER BY pubkey")
                .fetch_all(&self.account_db.inner.pool)
                .await
                .map_err(DatabaseError::Sqlx)?;

        let mut pubkeys = Vec::with_capacity(rows.len());
        for (hex,) in rows {
            let pk = PublicKey::from_hex(&hex).map_err(|e| {
                DatabaseError::Sqlx(sqlx::Error::Decode(
                    format!("Invalid follow pubkey {hex}: {e}").into(),
                ))
            })?;
            pubkeys.push(pk);
        }
        Ok(pubkeys)
    }

    /// Return all users followed by this account.
    ///
    /// Looks up followed pubkeys from the per-account DB, then batch-resolves
    /// them to [`User`] rows in the shared DB. Pubkeys without a corresponding
    /// `users` row are omitted (callers that need every pubkey should use
    /// [`Self::follow_pubkeys`] directly).
    pub async fn all(&self) -> Result<Vec<User>> {
        let pubkeys = self.follow_pubkeys().await?;
        if pubkeys.is_empty() {
            return Ok(Vec::new());
        }
        let users = User::find_by_pubkeys(&pubkeys, &self.shared_db).await?;
        if users.len() < pubkeys.len() {
            tracing::debug!(
                target: "whitenoise::database::account::follows",
                followed = pubkeys.len(),
                resolved = users.len(),
                missing = pubkeys.len() - users.len(),
                "Some followed pubkeys have no User row yet (metadata not fetched); \
                 use follow_pubkeys() for the full follow set"
            );
        }
        Ok(users)
    }

    /// Return whether this account follows `target_pubkey`.
    pub async fn is_following(&self, target_pubkey: &PublicKey) -> Result<bool> {
        let row: Option<(bool,)> =
            sqlx::query_as("SELECT EXISTS(SELECT 1 FROM account_follows WHERE pubkey = ?)")
                .bind(target_pubkey.to_hex())
                .fetch_optional(&self.account_db.inner.pool)
                .await
                .map_err(DatabaseError::Sqlx)?;
        Ok(row.map(|(exists,)| exists).unwrap_or(false))
    }

    /// Record that this account follows `user`. Idempotent — already-followed
    /// pubkeys just bump `updated_at`.
    pub async fn add(&self, user: &User) -> Result<()> {
        self.add_pubkey(&user.pubkey).await
    }

    /// Record that this account follows `pubkey`. Idempotent.
    pub async fn add_pubkey(&self, pubkey: &PublicKey) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO account_follows (pubkey, created_at, updated_at) \
             VALUES (?, ?, ?) \
             ON CONFLICT(pubkey) DO UPDATE SET updated_at = ?",
        )
        .bind(pubkey.to_hex())
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&self.account_db.inner.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;
        Ok(())
    }

    /// Remove the follow relationship between this account and `user`.
    pub async fn remove(&self, user: &User) -> Result<()> {
        self.remove_pubkey(&user.pubkey).await
    }

    /// Remove the follow relationship between this account and `pubkey`.
    pub async fn remove_pubkey(&self, pubkey: &PublicKey) -> Result<()> {
        sqlx::query("DELETE FROM account_follows WHERE pubkey = ?")
            .bind(pubkey.to_hex())
            .execute(&self.account_db.inner.pool)
            .await
            .map_err(DatabaseError::Sqlx)?;
        Ok(())
    }

    /// Replace the entire follow set with `pubkeys` atomically.
    ///
    /// Deletes all existing rows and inserts new ones in a single transaction.
    /// Duplicates within `pubkeys` are coalesced via primary-key conflict.
    pub async fn replace_all(&self, pubkeys: &[PublicKey]) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut tx = self
            .account_db
            .inner
            .pool
            .begin()
            .await
            .map_err(DatabaseError::Sqlx)?;

        sqlx::query("DELETE FROM account_follows")
            .execute(&mut *tx)
            .await
            .map_err(DatabaseError::Sqlx)?;

        for pubkey in pubkeys {
            sqlx::query(
                "INSERT OR IGNORE INTO account_follows (pubkey, created_at, updated_at) \
                 VALUES (?, ?, ?)",
            )
            .bind(pubkey.to_hex())
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(DatabaseError::Sqlx)?;
        }

        tx.commit().await.map_err(DatabaseError::Sqlx)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nostr_sdk::Keys;
    use tempfile::TempDir;

    use super::AccountFollowsRepo;
    use crate::whitenoise::database::account_db::AccountDatabase;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use crate::whitenoise::users::User;

    async fn fresh_repo() -> (
        AccountFollowsRepo,
        TempDir,
        Arc<crate::whitenoise::Whitenoise>,
    ) {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let path = dir.path().join("acct.db");
        let account_db = Arc::new(AccountDatabase::new(pubkey, path).await.unwrap());

        sqlx::query("DROP TABLE IF EXISTS account_follows")
            .execute(&account_db.inner.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE account_follows (
                pubkey TEXT NOT NULL PRIMARY KEY,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(&account_db.inner.pool)
        .await
        .unwrap();

        let repo = AccountFollowsRepo::new(account_db, wn.shared.database.clone());
        (repo, dir, wn)
    }

    async fn ensure_user_exists(
        wn: &crate::whitenoise::Whitenoise,
        pubkey: &nostr_sdk::PublicKey,
    ) -> User {
        let (user, _) = User::find_or_create_by_pubkey(pubkey, &wn.shared.database)
            .await
            .unwrap();
        user
    }

    #[tokio::test]
    async fn all_returns_empty_for_new_account() {
        let (repo, _dir, _wn) = fresh_repo().await;
        assert!(repo.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_and_all_returns_followed_user() {
        let (repo, _dir, wn) = fresh_repo().await;
        let target_pk = Keys::generate().public_key();
        let target_user = ensure_user_exists(&wn, &target_pk).await;

        repo.add(&target_user).await.unwrap();

        let follows = repo.all().await.unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0].pubkey, target_pk);
    }

    #[tokio::test]
    async fn is_following_returns_false_when_not_following() {
        let (repo, _dir, _wn) = fresh_repo().await;
        let target_pk = Keys::generate().public_key();
        assert!(!repo.is_following(&target_pk).await.unwrap());
    }

    #[tokio::test]
    async fn add_then_is_following_returns_true() {
        let (repo, _dir, wn) = fresh_repo().await;
        let target_pk = Keys::generate().public_key();
        let target_user = ensure_user_exists(&wn, &target_pk).await;

        repo.add(&target_user).await.unwrap();
        assert!(repo.is_following(&target_pk).await.unwrap());
    }

    #[tokio::test]
    async fn remove_clears_follow() {
        let (repo, _dir, wn) = fresh_repo().await;
        let target_pk = Keys::generate().public_key();
        let target_user = ensure_user_exists(&wn, &target_pk).await;

        repo.add(&target_user).await.unwrap();
        repo.remove(&target_user).await.unwrap();

        assert!(!repo.is_following(&target_pk).await.unwrap());
        assert!(repo.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_is_idempotent() {
        let (repo, _dir, _wn) = fresh_repo().await;
        let target_pk = Keys::generate().public_key();

        repo.add_pubkey(&target_pk).await.unwrap();
        repo.add_pubkey(&target_pk).await.unwrap();

        assert_eq!(repo.follow_pubkeys().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn replace_all_overwrites_existing() {
        let (repo, _dir, _wn) = fresh_repo().await;
        let pk_a = Keys::generate().public_key();
        let pk_b = Keys::generate().public_key();
        let pk_c = Keys::generate().public_key();

        repo.replace_all(&[pk_a, pk_b]).await.unwrap();
        let mut after_first = repo.follow_pubkeys().await.unwrap();
        after_first.sort();
        let mut want = vec![pk_a, pk_b];
        want.sort();
        assert_eq!(after_first, want);

        repo.replace_all(&[pk_c]).await.unwrap();
        let after_second = repo.follow_pubkeys().await.unwrap();
        assert_eq!(after_second, vec![pk_c]);
    }

    #[tokio::test]
    async fn replace_all_deduplicates() {
        let (repo, _dir, _wn) = fresh_repo().await;
        let pk = Keys::generate().public_key();

        repo.replace_all(&[pk, pk, pk]).await.unwrap();
        assert_eq!(repo.follow_pubkeys().await.unwrap().len(), 1);
    }
}
