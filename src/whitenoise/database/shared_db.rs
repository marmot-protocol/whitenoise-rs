//! Shared database wrapper — future home of the cross-account SQLite file.
//!
//! This is a scaffold for the Phase 18 database split. See
//! `docs/session-projection-implementation-plan.md` for the full table
//! ownership audit and migration plan.
//!
//! **Current status:** thin newtype over [`Database`]. No tables have moved
//! yet; the physical split happens in Phases 18b–18e.

use std::path::PathBuf;

use crate::whitenoise::database::{Database, DatabaseError};

/// A connection to the shared SQLite database.
///
/// Holds data that is not tied to any single account: `app_settings`,
/// `accounts` (registry), `users`, `relays`, `user_relays`,
/// `cached_graph_users`, `group_information`, `processed_events` (global),
/// `media_blobs`, `relay_status`, and `relay_events`.
///
/// See `rearchitecture.md` Appendix B for the authoritative table ownership
/// map.
///
/// # Cross-scope FK audit
///
/// These FKs cross the shared/account boundary and require application-level
/// enforcement after the physical split. Phase 18c moved
/// `account_settings`, `drafts`, `published_key_packages`, `published_events`,
/// `account_follows`, message projections, and their supporting state out of
/// shared (their FKs are no longer relevant).
///
/// ## Pubkey-keyed FKs (application-level enforcement only — Phase 18d)
///
/// - `accounts_groups.account_pubkey → accounts.pubkey` (migration 0018)
/// - `media_files.account_pubkey → accounts.pubkey` (migration 0015)
/// - `push_registrations.account_pubkey → accounts.pubkey` (migration 0039)
/// - `group_push_tokens.account_pubkey → accounts.pubkey` (migration 0041)
///
/// ## Cross-scope data references (no schema FK, but logical dependency)
///
/// - `aggregated_messages.mls_group_id → group_information.mls_group_id`
///   (`aggregated_messages` is account-scoped; `group_information` is shared)
/// - `media_references.encrypted_file_hash → media_blobs.hash`
///   (`media_references` is account-scoped; `media_blobs` is shared)
///
/// # Current implementation
///
/// Thin newtype wrapper around [`Database`]. The split into a separate file
/// happens in Phases 18b–18e.
#[derive(Clone, Debug)]
pub struct SharedDatabase {
    /// Underlying connection; will become its own `shared.db` pool post-split.
    pub(crate) inner: Database,
}

impl SharedDatabase {
    /// Open (or create) the shared database at `db_path`.
    ///
    /// Runs all pending Rust migrations on first open.
    pub async fn new(db_path: PathBuf) -> Result<Self, DatabaseError> {
        let inner = Database::new(db_path).await?;
        Ok(Self { inner })
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

    #[tokio::test]
    async fn test_shared_database_opens_and_migrates() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("shared.db");

        let db = SharedDatabase::new(path.clone()).await;
        assert!(db.is_ok(), "expected Ok, got {db:?}");

        let db = db.unwrap();
        assert_eq!(db.path(), &path);
    }

    #[tokio::test]
    async fn test_shared_database_is_idempotent_across_opens() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("shared.db");

        // First open — creates file and runs migrations.
        let _ = SharedDatabase::new(path.clone()).await.expect("first open");

        // Second open — should not fail (migrations already applied).
        let db = SharedDatabase::new(path.clone()).await;
        assert!(db.is_ok(), "expected Ok on second open, got {db:?}");
    }
}
