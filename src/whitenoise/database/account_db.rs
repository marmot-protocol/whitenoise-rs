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
/// key. Account-scoped tables: `account_settings`, `account_follows`,
/// `accounts_groups`, `drafts`, `push_registrations`, `group_push_tokens`,
/// `published_key_packages`, `published_events`,
/// `processed_events` (account-level), `aggregated_messages`,
/// `message_delivery_status`, and `media_references`.
///
/// See `rearchitecture.md` Appendix B for the authoritative table ownership
/// map.
///
/// # Cross-scope FK audit
///
/// These FKs cross the account/shared boundary and will require
/// application-level enforcement after the physical split in Phases 18b–18e:
///
/// ## Integer-keyed FKs (require schema migration before split)
///
/// These use `accounts(id)` (integer rowid) as the FK target. Integer IDs
/// from `shared.sqlite` won't transfer meaningfully to per-account files,
/// so these tables need a schema migration to switch to pubkey-keyed FKs
/// before the physical split:
///
/// - `published_events.account_id → accounts(id)` (migration 0011)
/// - `processed_events.account_id → accounts(id)` (migration 0012)
///
/// ## Pubkey-keyed FKs (application-level enforcement only)
///
/// - `accounts_groups.account_pubkey → accounts.pubkey` (migration 0018)
/// - `account_settings.account_pubkey → accounts.pubkey`
/// - `media_files.account_pubkey → accounts.pubkey` (migration 0015)
/// - `push_registrations.account_pubkey → accounts.pubkey` (migration 0039)
/// - `group_push_tokens.account_pubkey → accounts.pubkey` (migration 0041)
/// - `published_key_packages.account_pubkey → accounts.pubkey` (migration 0029)
/// - `drafts.account_pubkey → accounts.pubkey` (migration 0028)
///
/// ## Cross-scope data references (no schema FK, but logical dependency)
///
/// - `aggregated_messages.mls_group_id → group_information.mls_group_id`
///   (`group_information` is shared; `aggregated_messages` is account-scoped)
/// - `media_references.encrypted_file_hash → media_blobs.hash`
///   (`media_blobs` is shared; `media_references` is account-scoped)
/// - `drafts.mls_group_id → group_information.mls_group_id`
///   (`group_information` is shared; `drafts` is account-scoped;
///   migration 0028)
///
/// ## Intra-account FKs (no cross-boundary issue)
///
/// - `message_delivery_status.aggregated_message_id → aggregated_messages.id`
///   (both account-scoped — will stay together)
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
