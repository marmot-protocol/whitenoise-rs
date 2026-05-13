//! Per-account repository for push registrations.
//!
//! Wraps the existing [`PushRegistration`] DB functions so that callers do not
//! need to thread an `account_pubkey` argument through every call — the pubkey
//! is baked in at construction time.

use std::sync::Arc;

use nostr_sdk::{PublicKey, RelayUrl};

use crate::whitenoise::database::account_db::AccountDatabase;
use crate::whitenoise::error::Result;
use crate::whitenoise::push_notifications::{PushPlatform, PushRegistration};

/// Repository for push registrations scoped to a single account.
#[derive(Clone, Debug)]
pub struct PushRegistrationsRepo {
    account_pubkey: PublicKey,
    db: Arc<AccountDatabase>,
}

impl PushRegistrationsRepo {
    /// Construct a new [`PushRegistrationsRepo`] for `account_pubkey`.
    pub(crate) fn new(account_pubkey: PublicKey, db: Arc<AccountDatabase>) -> Self {
        Self { account_pubkey, db }
    }

    /// Return the push registration for this account, if one exists.
    pub async fn find(&self) -> Result<Option<PushRegistration>> {
        Ok(PushRegistration::find(&self.account_pubkey, &self.db.inner.pool).await?)
    }

    /// Create or replace the push registration for this account.
    pub async fn upsert(
        &self,
        platform: PushPlatform,
        raw_token: &str,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
    ) -> Result<PushRegistration> {
        Ok(PushRegistration::upsert(
            &self.account_pubkey,
            platform,
            raw_token,
            server_pubkey,
            relay_hint,
            &self.db.inner.pool,
        )
        .await?)
    }

    /// Delete the push registration for this account. Returns `true` if a row
    /// was removed.
    pub async fn delete(&self) -> Result<bool> {
        Ok(PushRegistration::delete(&self.db.inner.pool).await?)
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;
    use tempfile::TempDir;

    use super::*;

    async fn setup() -> (PushRegistrationsRepo, PublicKey, TempDir) {
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let db = Arc::new(
            AccountDatabase::new(pubkey, dir.path().join("acct.db"))
                .await
                .unwrap(),
        );
        // Matches the project-wide account-DB test pattern: stamp the schema
        // directly because `AccountDatabase::new` uses `open_without_migrations`
        // and running the full migration timeline here would require wiring
        // a shared pool.
        sqlx::query("DROP TABLE IF EXISTS push_registrations")
            .execute(&db.inner.pool)
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
        .execute(&db.inner.pool)
        .await
        .unwrap();
        (PushRegistrationsRepo::new(pubkey, db), pubkey, dir)
    }

    /// The wrapper's only added behavior is binding `account_pubkey` to the
    /// returned record. Inner [`PushRegistration`] tests cover the SQL semantics.
    #[tokio::test]
    async fn upsert_binds_repo_account_pubkey_to_returned_registration() {
        let (repo, expected_pubkey, _dir) = setup().await;
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        let inserted = repo
            .upsert(
                PushPlatform::Apns,
                "token-aaaa",
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();
        assert_eq!(
            inserted.account_pubkey, expected_pubkey,
            "upsert must bind the repo's account_pubkey to the returned registration"
        );
    }

    #[tokio::test]
    async fn separate_accounts_have_independent_per_account_dbs() {
        // The per-account DB IS the scoping mechanism. Two repos over two
        // distinct account DBs must not see each other's registrations.
        let (repo_a, pubkey_a, _dir_a) = setup().await;
        let (repo_b, pubkey_b, _dir_b) = setup().await;
        assert_ne!(pubkey_a, pubkey_b);

        let server = Keys::generate().public_key();
        repo_a
            .upsert(PushPlatform::Apns, "token-a", &server, None)
            .await
            .unwrap();

        let in_a = repo_a.find().await.unwrap().expect("A must see its row");
        assert_eq!(in_a.account_pubkey, pubkey_a);
        assert_eq!(in_a.raw_token, "token-a");

        let in_b = repo_b.find().await.unwrap();
        assert!(in_b.is_none(), "B must not see A's registration");
    }
}
