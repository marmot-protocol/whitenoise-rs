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
/// Holds data that is not tied to any single account: users, relays,
/// group information, aggregated messages, app settings, and the media blob
/// cache. See the implementation plan for the full table list.
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
    /// Runs all pending migrations in `./db_migrations` on first open.
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
