//! Per-account repository for cached group push tokens.
//!
//! Wraps the existing [`GroupPushToken`] DB functions so that callers do not
//! need to thread an `account_pubkey` argument through every call — the pubkey
//! is baked in at construction time.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::marmot::GroupId;
use nostr_sdk::{PublicKey, RelayUrl};

use crate::marmot::push::LeafTokenTag;
use crate::whitenoise::database::account_db::AccountDatabase;
use crate::whitenoise::database::group_push_tokens::GroupPushTokenUpsert;
use crate::whitenoise::error::Result;
use crate::whitenoise::push_notifications::GroupPushToken;

/// Repository for group push tokens scoped to a single account.
#[derive(Clone, Debug)]
pub struct GroupPushTokensRepo {
    account_pubkey: PublicKey,
    db: Arc<AccountDatabase>,
}

impl GroupPushTokensRepo {
    /// Construct a new [`GroupPushTokensRepo`] for `account_pubkey`.
    pub(crate) fn new(account_pubkey: PublicKey, db: Arc<AccountDatabase>) -> Self {
        Self { account_pubkey, db }
    }

    /// Return all cached push tokens for a specific group.
    pub async fn find_by_group(&self, group_id: &GroupId) -> Result<Vec<GroupPushToken>> {
        Ok(GroupPushToken::find_by_account_and_group(
            &self.account_pubkey,
            group_id,
            &self.db.inner.pool,
        )
        .await?)
    }

    /// Insert or update a single cached push token for a group member.
    pub async fn upsert(
        &self,
        group_id: &GroupId,
        member_pubkey: &PublicKey,
        leaf_index: u32,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
        encrypted_token: &str,
    ) -> Result<GroupPushToken> {
        Ok(GroupPushToken::upsert(
            &self.account_pubkey,
            group_id,
            member_pubkey,
            leaf_index,
            server_pubkey,
            relay_hint,
            encrypted_token,
            &self.db.inner.pool,
        )
        .await?)
    }

    /// Insert or update a cached push token while preserving current
    /// Darkmatter push-gossip metadata.
    pub(crate) async fn upsert_with_metadata(
        &self,
        upsert: GroupPushTokenUpsert<'_>,
    ) -> Result<GroupPushToken> {
        Ok(
            GroupPushToken::upsert_with_metadata(&self.account_pubkey, upsert, &self.db.inner.pool)
                .await?,
        )
    }

    /// Delete the cached token for a specific leaf index in a group.
    pub async fn delete(&self, group_id: &GroupId, leaf_index: u32) -> Result<bool> {
        Ok(GroupPushToken::delete(group_id, leaf_index, &self.db.inner.pool).await?)
    }

    /// Delete all cached tokens for a specific member in a group.
    pub async fn delete_by_member_pubkey(
        &self,
        group_id: &GroupId,
        member_pubkey: &PublicKey,
    ) -> Result<bool> {
        Ok(
            GroupPushToken::delete_by_member_pubkey(group_id, member_pubkey, &self.db.inner.pool)
                .await?,
        )
    }

    /// Bulk upsert tokens from a token-list response, filtering to active
    /// leaves only.
    pub async fn upsert_active_token_list_response(
        &self,
        group_id: &GroupId,
        active_leaf_map: &BTreeMap<u32, PublicKey>,
        tokens: Vec<LeafTokenTag>,
    ) -> Result<()> {
        Ok(GroupPushToken::upsert_active_token_list_response(
            group_id,
            active_leaf_map,
            tokens,
            &self.db.inner.pool,
        )
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;
    use tempfile::TempDir;

    use super::*;

    fn make_group_id(byte: u8) -> GroupId {
        GroupId::from_slice(&[byte; 32])
    }

    async fn setup() -> (GroupPushTokensRepo, PublicKey, TempDir) {
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
        // a shared pool. NOTE: the CREATE TABLE below must stay in sync with
        // `fresh_account_schema.sql`; until a `setup_account_db_with_migrations`
        // helper exists, divergence here will silently mask production drift.
        sqlx::query("DROP TABLE IF EXISTS group_push_tokens")
            .execute(&db.inner.pool)
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
        .execute(&db.inner.pool)
        .await
        .unwrap();
        (GroupPushTokensRepo::new(pubkey, db), pubkey, dir)
    }

    /// The wrapper's only added behavior is binding `account_pubkey` to the
    /// returned record. Inner [`GroupPushToken`] tests cover the SQL semantics.
    #[tokio::test]
    async fn upsert_binds_repo_account_pubkey_to_returned_token() {
        let (repo, account_pubkey, _dir) = setup().await;
        let group_id = make_group_id(2);
        let member = Keys::generate().public_key();
        let server = Keys::generate().public_key();
        let hint = RelayUrl::parse("wss://push.example.com").unwrap();

        let inserted = repo
            .upsert(&group_id, &member, 0, &server, Some(&hint), "ENCRYPTED")
            .await
            .unwrap();
        assert_eq!(
            inserted.account_pubkey, account_pubkey,
            "upsert must bind the repo's account_pubkey to the returned token"
        );
    }
}
