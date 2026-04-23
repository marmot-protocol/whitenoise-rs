//! Per-account database wrapper — future home of the account-scoped SQLite file.
//!
//! This is a scaffold for the Phase 18 database split. See
//! `docs/session-projection-implementation-plan.md` for the full table
//! ownership audit and migration plan.
//!
//! **Current status:** thin newtype over [`Database`]. No tables have moved
//! yet; the physical split happens in Phases 18b–18e.

use std::path::PathBuf;

use nostr_sdk::PublicKey;

use crate::whitenoise::database::{Database, DatabaseError};

/// A connection to a per-account SQLite database.
///
/// Each account will have its own file, identified by the account's hex public
/// key. Account-scoped tables (settings, follows, drafts, key packages,
/// published events, group membership, push tokens) will live here.
///
/// # Current implementation
///
/// Thin newtype wrapper around [`Database`]. The split into separate files
/// happens in Phases 18b–18e.
#[derive(Clone, Debug)]
pub struct AccountDatabase {
    /// The account this database belongs to.
    pub(crate) account_pubkey: PublicKey,
    /// Underlying connection; will become its own `<pubkey>.db` pool post-split.
    pub(crate) inner: Database,
}

impl AccountDatabase {
    /// Open (or create) the account database for `account_pubkey` at `db_path`.
    ///
    /// Runs all pending migrations on first open.
    pub async fn new(account_pubkey: PublicKey, db_path: PathBuf) -> Result<Self, DatabaseError> {
        let inner = Database::new(db_path).await?;
        Ok(Self {
            account_pubkey,
            inner,
        })
    }

    /// The public key that owns this database.
    pub fn account_pubkey(&self) -> &PublicKey {
        &self.account_pubkey
    }

    /// Path to the underlying SQLite file.
    pub fn path(&self) -> &PathBuf {
        &self.inner.path
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn test_pubkey() -> PublicKey {
        PublicKey::from_hex("000000000000000000000000000000000000000000000000000000000000cafe")
            .expect("valid test pubkey")
    }

    #[tokio::test]
    async fn test_account_database_opens_and_migrates() {
        let dir = TempDir::new().expect("temp dir");
        let pubkey = test_pubkey();
        let path = dir.path().join(format!("{}.db", pubkey.to_hex()));

        let db = AccountDatabase::new(pubkey, path.clone()).await;
        assert!(db.is_ok(), "expected Ok, got {db:?}");

        let db = db.unwrap();
        assert_eq!(db.path(), &path);
        assert_eq!(db.account_pubkey(), &test_pubkey());
    }

    #[tokio::test]
    async fn test_account_database_is_idempotent_across_opens() {
        let dir = TempDir::new().expect("temp dir");
        let pubkey = test_pubkey();
        let path = dir.path().join(format!("{}.db", pubkey.to_hex()));

        // First open.
        let _ = AccountDatabase::new(pubkey, path.clone())
            .await
            .expect("first open");

        // Second open — migrations already applied.
        let db = AccountDatabase::new(test_pubkey(), path).await;
        assert!(db.is_ok(), "expected Ok on second open, got {db:?}");
    }
}
